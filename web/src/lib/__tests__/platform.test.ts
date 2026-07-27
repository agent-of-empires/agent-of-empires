// @vitest-environment jsdom

import { afterEach, describe, expect, it, vi } from "vitest";

import { isIOS, isStandalone } from "../platform";

afterEach(() => {
  vi.restoreAllMocks();
  // jsdom has neither of these by default; clear any per-test stubs.
  delete (window.navigator as unknown as { standalone?: boolean }).standalone;
  delete (window as unknown as { matchMedia?: unknown }).matchMedia;
});

function setUserAgent(ua: string) {
  vi.spyOn(navigator, "userAgent", "get").mockReturnValue(ua);
}

function stubMatchMedia(standaloneMatches: boolean) {
  Object.defineProperty(window, "matchMedia", {
    configurable: true,
    value: (q: string) => ({ matches: standaloneMatches && q === "(display-mode: standalone)" }) as MediaQueryList,
  });
}

describe("isIOS", () => {
  it("is true for an iPhone userAgent", () => {
    setUserAgent("Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X) AppleWebKit/605.1.15");
    expect(isIOS()).toBe(true);
  });

  it("is false for a desktop userAgent", () => {
    setUserAgent("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15) AppleWebKit/537.36");
    expect(isIOS()).toBe(false);
  });
});

describe("isStandalone", () => {
  it("is true when navigator.standalone is set (iOS installed PWA)", () => {
    Object.defineProperty(window.navigator, "standalone", { configurable: true, value: true });
    stubMatchMedia(false);
    expect(isStandalone()).toBe(true);
  });

  it("is true when the standalone display-mode media query matches", () => {
    stubMatchMedia(true);
    expect(isStandalone()).toBe(true);
  });

  it("is false in a plain browser tab", () => {
    stubMatchMedia(false);
    expect(isStandalone()).toBe(false);
  });
});
