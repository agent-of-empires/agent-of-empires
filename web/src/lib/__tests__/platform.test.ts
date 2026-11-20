// @vitest-environment jsdom

import { afterEach, describe, expect, it, vi } from "vitest";

import { installHint, isIOS, isStandalone } from "../platform";

const realLocation = window.location;

afterEach(() => {
  vi.restoreAllMocks();
  delete (window.navigator as unknown as { standalone?: boolean }).standalone;
  delete (window as unknown as { matchMedia?: unknown }).matchMedia;
  delete (window as { isSecureContext?: boolean }).isSecureContext;
  Object.defineProperty(window, "location", { configurable: true, value: realLocation });
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

const IPHONE = "Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X) AppleWebKit/605.1.15";
const IPAD = "Mozilla/5.0 (iPad; CPU OS 17_0 like Mac OS X) AppleWebKit/605.1.15";
const ANDROID = "Mozilla/5.0 (Linux; Android 14; Pixel 8) AppleWebKit/537.36 Chrome/126.0 Mobile Safari/537.36";
const FIREFOX_TOUCH = "Mozilla/5.0 (X11; Linux x86_64; rv:128.0) Gecko/20100101 Firefox/128.0";

describe("installHint", () => {
  it.each([
    { name: "an iPhone Safari tab", ua: IPHONE, standalone: false, finePointer: false, secure: true, want: "ios" },
    { name: "the installed iPhone app", ua: IPHONE, standalone: true, finePointer: false, secure: true, want: null },
    {
      name: "an Android Chrome tab",
      ua: ANDROID,
      standalone: false,
      finePointer: false,
      secure: true,
      want: "android",
    },
    {
      name: "another touch browser",
      ua: FIREFOX_TOUCH,
      standalone: false,
      finePointer: false,
      secure: true,
      want: "other",
    },
    { name: "a desktop", ua: FIREFOX_TOUCH, standalone: false, finePointer: true, secure: true, want: null },
    // Treated as a desktop on purpose: it can follow the replay line like one.
    { name: "an iPad with a trackpad", ua: IPAD, standalone: false, finePointer: true, secure: true, want: null },
    { name: "plain HTTP on a LAN", ua: ANDROID, standalone: false, finePointer: false, secure: false, want: null },
  ])("is $want for $name", ({ ua, standalone, finePointer, secure, want }) => {
    setUserAgent(ua);
    Object.defineProperty(window, "matchMedia", {
      configurable: true,
      value: (q: string) =>
        ({
          matches:
            (standalone && q === "(display-mode: standalone)") ||
            (finePointer && q === "(hover: hover) and (pointer: fine)"),
        }) as MediaQueryList,
    });
    Object.defineProperty(window, "isSecureContext", { configurable: true, value: secure });
    if (!secure) Object.defineProperty(window, "location", { configurable: true, value: { hostname: "192.168.1.20" } });
    expect(installHint()).toBe(want);
  });
});
