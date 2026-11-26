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
  delete (navigator as { serviceWorker?: unknown }).serviceWorker;
  delete (window as { PushManager?: unknown }).PushManager;
  delete (window as { Notification?: unknown }).Notification;
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
  it.each([
    {
      name: "an iPhone",
      ua: "Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X) AppleWebKit/605.1.15",
      touch: 5,
      want: true,
    },
    {
      name: "an iPad in desktop mode",
      ua: "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15",
      touch: 5,
      want: true,
    },
    { name: "a Mac", ua: "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15) AppleWebKit/537.36", touch: 0, want: false },
  ])("is $want for $name", ({ ua, touch, want }) => {
    setUserAgent(ua);
    Object.defineProperty(navigator, "maxTouchPoints", { configurable: true, value: touch });
    expect(isIOS()).toBe(want);
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
const LINUX_FIREFOX = "Mozilla/5.0 (X11; Linux x86_64; rv:128.0) Gecko/20100101 Firefox/128.0";

describe("installHint", () => {
  const base = { touch: 5, standalone: false, finePointer: false, secure: true, push: false };
  it.each(
    [
      { name: "an iPhone Safari tab", ua: IPHONE, want: "ios" },
      // iOS Safari exposes push only to Home Screen apps, so a tab that has it is still told to install.
      { name: "an iPhone tab that exposes push", ua: IPHONE, push: true, want: "ios" },
      { name: "the installed iPhone app", ua: IPHONE, standalone: true, want: null },
      { name: "an iPad with a trackpad", ua: IPAD, finePointer: true, want: "ios" },
      { name: "an iPad in desktop mode", ua: MAC, want: "ios" },
      { name: "an Android Chrome tab", ua: ANDROID, push: true, want: "push" },
      { name: "an Android device with a mouse", ua: ANDROID, finePointer: true, push: true, want: "push" },
      { name: "an Android browser without push", ua: ANDROID, want: "other" },
      // Touch points, not a coarse pointer, make it a touch device; push support picks the line.
      { name: "a touch-only Linux browser that can push", ua: LINUX_FIREFOX, push: true, want: "push" },
      { name: "a touch-only Linux browser without push", ua: LINUX_FIREFOX, want: "other" },
      { name: "a desktop", ua: LINUX_FIREFOX, touch: 0, finePointer: true, push: true, want: null },
      {
        name: "a desktop with a touchscreen and a mouse",
        ua: LINUX_FIREFOX,
        finePointer: true,
        push: true,
        want: null,
      },
      { name: "a desktop with no pointing device or touch", ua: LINUX_FIREFOX, touch: 0, want: null },
      { name: "a Mac", ua: MAC, touch: 0, finePointer: true, push: true, want: null },
      { name: "plain HTTP on a LAN", ua: ANDROID, secure: false, push: true, want: null },
    ].map((row) => ({ ...base, ...row })),
  )("is $want for $name", ({ ua, touch, standalone, finePointer, secure, push, want }) => {
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
    if (push) {
      Object.defineProperty(navigator, "serviceWorker", { configurable: true, value: {} });
      Object.defineProperty(window, "PushManager", { configurable: true, value: function PushManager() {} });
      Object.defineProperty(window, "Notification", { configurable: true, value: function Notification() {} });
    }
    Object.defineProperty(window, "isSecureContext", { configurable: true, value: secure });
    if (!secure) Object.defineProperty(window, "location", { configurable: true, value: { hostname: "192.168.1.20" } });
    expect(installHint()).toBe(want);
  });
});
