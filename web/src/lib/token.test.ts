// @vitest-environment jsdom
import { afterEach, beforeEach, expect, it, vi } from "vitest";

import { getToken, saveToken, clearToken } from "./token";

beforeEach(() => localStorage.clear());
afterEach(() => {
  localStorage.clear();
  vi.restoreAllMocks();
});

it("persists a trimmed token, ignores blank ones, and clears it", () => {
  expect(getToken()).toBeNull();
  saveToken("  abc123  ");
  expect(localStorage.getItem("aoe_auth_token")).toBe("abc123");
  saveToken("   ");
  expect(getToken()).toBe("abc123");
  clearToken();
  expect(getToken()).toBeNull();
});

it("tolerates storage that throws", () => {
  for (const method of ["getItem", "setItem", "removeItem"] as const) {
    vi.spyOn(Storage.prototype, method).mockImplementation(() => {
      throw new Error("blocked");
    });
  }
  expect(getToken()).toBeNull();
  expect(() => saveToken("abc123")).not.toThrow();
  expect(() => clearToken()).not.toThrow();
});
