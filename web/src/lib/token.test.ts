// @vitest-environment jsdom
//
// Coverage for the auth-token persistence helpers. The module captures a
// `?token=` from the URL at import time; here we exercise the runtime getters
// and setters: getToken reads localStorage, saveToken trims and skips empty,
// clearToken removes the key.

import { afterEach, beforeEach, describe, expect, it } from "vitest";

import { getToken, saveToken, clearToken } from "./token";

const STORAGE_KEY = "aoe_auth_token";

beforeEach(() => localStorage.clear());
afterEach(() => localStorage.clear());

describe("token helpers", () => {
  it("getToken returns null when unset", () => {
    expect(getToken()).toBeNull();
  });

  it("saveToken persists and getToken reads it back", () => {
    saveToken("abc123");
    expect(localStorage.getItem(STORAGE_KEY)).toBe("abc123");
    expect(getToken()).toBe("abc123");
  });

  it("saveToken trims surrounding whitespace", () => {
    saveToken("  spaced  ");
    expect(getToken()).toBe("spaced");
  });

  it("saveToken ignores an empty / whitespace-only token", () => {
    saveToken("seed");
    saveToken("   ");
    expect(getToken()).toBe("seed");
  });

  it("clearToken removes the stored token", () => {
    saveToken("gone");
    clearToken();
    expect(getToken()).toBeNull();
  });
});
