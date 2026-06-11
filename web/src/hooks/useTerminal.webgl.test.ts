// @vitest-environment jsdom
// Renderer gate: the WebGL addon must not load on WebKit. Safari 26.x
// garbles xterm's WebGL glyph atlas (xtermjs/xterm.js#5816) and iOS
// WebKit (which backs every iOS browser) tears down GL contexts when a
// PWA is backgrounded. See shouldUseWebglRenderer in useTerminal.ts.
import { describe, expect, it } from "vitest";
import { shouldUseWebglRenderer } from "./useTerminal";

const UA = {
  iphoneSafari:
    "Mozilla/5.0 (iPhone; CPU iPhone OS 26_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/26.0 Mobile/15E148 Safari/604.1",
  iphoneChrome:
    "Mozilla/5.0 (iPhone; CPU iPhone OS 26_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) CriOS/126.0.6478.54 Mobile/15E148 Safari/604.1",
  // iPadOS 13+ masquerades as desktop macOS; only maxTouchPoints gives it away.
  ipadDesktopMode:
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/26.0 Safari/605.1.15",
  macSafari:
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/26.0 Safari/605.1.15",
  macChrome:
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36",
  windowsEdge:
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36 Edg/126.0.0.0",
  linuxFirefox: "Mozilla/5.0 (X11; Linux x86_64; rv:127.0) Gecko/20100101 Firefox/127.0",
  androidChrome:
    "Mozilla/5.0 (Linux; Android 14; Pixel 8) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.6478.71 Mobile Safari/537.36",
} as const;

describe("shouldUseWebglRenderer", () => {
  it("disables WebGL on iPhone Safari", () => {
    expect(shouldUseWebglRenderer(UA.iphoneSafari, "iPhone", 5)).toBe(false);
  });

  it("disables WebGL on iPhone Chrome (CriOS is WebKit too)", () => {
    expect(shouldUseWebglRenderer(UA.iphoneChrome, "iPhone", 5)).toBe(false);
  });

  it("disables WebGL on iPadOS masquerading as macOS", () => {
    expect(shouldUseWebglRenderer(UA.ipadDesktopMode, "MacIntel", 5)).toBe(false);
  });

  it("disables WebGL on desktop Safari", () => {
    expect(shouldUseWebglRenderer(UA.macSafari, "MacIntel", 0)).toBe(false);
  });

  it("keeps WebGL on desktop Chrome", () => {
    expect(shouldUseWebglRenderer(UA.macChrome, "MacIntel", 0)).toBe(true);
  });

  it("keeps WebGL on Windows Edge", () => {
    expect(shouldUseWebglRenderer(UA.windowsEdge, "Win32", 0)).toBe(true);
  });

  it("keeps WebGL on Linux Firefox", () => {
    expect(shouldUseWebglRenderer(UA.linuxFirefox, "Linux x86_64", 0)).toBe(true);
  });

  it("keeps WebGL on Android Chrome", () => {
    expect(shouldUseWebglRenderer(UA.androidChrome, "Linux armv81", 5)).toBe(true);
  });
});
