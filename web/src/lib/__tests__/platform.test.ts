// @vitest-environment jsdom

import { afterEach, describe, expect, it, vi } from "vitest";

import { installHint, isIOS, isStandalone } from "../platform";

const realLocation = window.location;

afterEach(() => {
  vi.restoreAllMocks();
  delete (window.navigator as unknown as { standalone?: boolean }).standalone;
  delete (window as unknown as { matchMedia?: unknown }).matchMedia;
  delete (window as { isSecureContext?: boolean }).isSecureContext;
  delete (navigator as { maxTouchPoints?: number }).maxTouchPoints;
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
const MAC = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 Version/17.0 Safari/605.1.15";
const FIREFOX_TOUCH = "Mozilla/5.0 (X11; Linux x86_64; rv:128.0) Gecko/20100101 Firefox/128.0";

describe("installHint", () => {
  it.each([
    {
      name: "an iPhone Safari tab",
      ua: IPHONE,
      touch: 5,
      standalone: false,
      finePointer: false,
      secure: true,
      want: "ios",
    },
    {
      name: "the installed iPhone app",
      ua: IPHONE,
      touch: 5,
      standalone: true,
      finePointer: false,
      secure: true,
      want: null,
    },
    {
      name: "an Android Chrome tab",
      ua: ANDROID,
      touch: 5,
      standalone: false,
      finePointer: false,
      secure: true,
      want: "android",
    },
    {
      name: "another touch browser",
      ua: FIREFOX_TOUCH,
      touch: 5,
      standalone: false,
      finePointer: false,
      secure: true,
      want: "other",
    },
    { name: "a desktop", ua: FIREFOX_TOUCH, touch: 0, standalone: false, finePointer: true, secure: true, want: null },
    // Push on an iPad needs the installed app too, whatever the pointer.
    {
      name: "an iPad with a trackpad",
      ua: IPAD,
      touch: 5,
      standalone: false,
      finePointer: true,
      secure: true,
      want: "ios",
    },
    {
      name: "an iPad in desktop mode",
      ua: MAC,
      touch: 5,
      standalone: false,
      finePointer: false,
      secure: true,
      want: "ios",
    },
    { name: "a Mac", ua: MAC, touch: 0, standalone: false, finePointer: true, secure: true, want: null },
    {
      name: "plain HTTP on a LAN",
      ua: ANDROID,
      touch: 5,
      standalone: false,
      finePointer: false,
      secure: false,
      want: null,
    },
  ])("is $want for $name", ({ ua, touch, standalone, finePointer, secure, want }) => {
    setUserAgent(ua);
    Object.defineProperty(navigator, "maxTouchPoints", { configurable: true, value: touch });
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
