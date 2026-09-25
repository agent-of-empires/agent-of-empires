// @vitest-environment jsdom
//
// Tests for the device-binding secret persisted in localStorage and
// shipped on every authenticated request to the dashboard. See #1131.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import {
  __getCachedDeviceBindingSecretForTests,
  __resetDeviceBindingForTests,
  clearDeviceBindingSecret,
  getOrCreateDeviceBindingSecret,
} from "./deviceBinding";

const STORAGE_KEY = "aoe_device_binding_secret_v1";

beforeEach(() => {
  window.localStorage.clear();
  __resetDeviceBindingForTests();
});

afterEach(() => {
  vi.restoreAllMocks();
});

describe("getOrCreateDeviceBindingSecret", () => {
  it("generates a base64url secret once, persists it, and rehydrates it after a cold cache", () => {
    const secret = getOrCreateDeviceBindingSecret();
    expect(secret).toMatch(/^[A-Za-z0-9_-]{43}=?$/);
    expect(window.localStorage.getItem(STORAGE_KEY)).toBe(secret);
    expect(getOrCreateDeviceBindingSecret()).toBe(secret);
    __resetDeviceBindingForTests();
    expect(__getCachedDeviceBindingSecretForTests()).toBeNull();
    expect(getOrCreateDeviceBindingSecret()).toBe(secret);
  });

  it("regenerates on garbled stored value", () => {
    window.localStorage.setItem(STORAGE_KEY, "not-valid-base64url-secret!");
    const fresh = getOrCreateDeviceBindingSecret();
    expect(fresh).toMatch(/^[A-Za-z0-9_-]{43}=?$/);
    expect(fresh).not.toBe("not-valid-base64url-secret!");
  });

  it("throws when crypto.getRandomValues is unavailable", () => {
    __resetDeviceBindingForTests();
    window.localStorage.clear();
    const originalCrypto = globalThis.crypto;
    Object.defineProperty(globalThis, "crypto", {
      configurable: true,
      value: undefined,
    });
    try {
      expect(() => getOrCreateDeviceBindingSecret()).toThrow(/crypto\.getRandomValues/);
    } finally {
      Object.defineProperty(globalThis, "crypto", {
        configurable: true,
        value: originalCrypto,
      });
    }
  });

  it("clearDeviceBindingSecret wipes storage and the cache", () => {
    getOrCreateDeviceBindingSecret();
    expect(window.localStorage.getItem(STORAGE_KEY)).not.toBeNull();
    clearDeviceBindingSecret();
    expect(window.localStorage.getItem(STORAGE_KEY)).toBeNull();
    expect(__getCachedDeviceBindingSecretForTests()).toBeNull();
  });
});
