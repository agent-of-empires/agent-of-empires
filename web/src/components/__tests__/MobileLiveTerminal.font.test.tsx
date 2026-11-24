// @vitest-environment jsdom

import { act } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { renderLiveTerminal } from "./liveTerminalHarness";

afterEach(() => {
  window.localStorage.clear();
  vi.restoreAllMocks();
  delete (document as { fonts?: unknown }).fonts;
});

describe("MobileLiveTerminal terminal font", () => {
  it("renders a bundled font with its own row height and re-measures once its face loads", async () => {
    window.localStorage.setItem("aoe-web-settings", JSON.stringify({ terminalFontFamily: "jetbrains mono" }));
    let loaded!: () => void;
    const load = vi.fn(() => new Promise<void>((resolve) => (loaded = resolve)));
    Object.defineProperty(document, "fonts", { configurable: true, value: { load, ready: Promise.resolve() } });
    let advance = 0;
    vi.spyOn(HTMLElement.prototype, "getBoundingClientRect").mockImplementation(
      () => ({ width: advance * 20 }) as DOMRect,
    );

    const { scroller } = renderLiveTerminal();
    expect(scroller.style.fontFamily).toBe('"JetBrains Mono", var(--font-mono)');
    // 14px at JetBrains Mono's 1.32 ratio.
    expect(scroller.style.lineHeight).toBe("18px");
    expect(load).toHaveBeenCalledWith('16px "JetBrains Mono"');

    advance = 8.5;
    await act(async () => loaded());
    expect(scroller.style.getPropertyValue("--term-cell")).toBe("8.5px");
  });

  it("re-measures the fallback when the chosen face fails to load", async () => {
    window.localStorage.setItem("aoe-web-settings", JSON.stringify({ terminalFontFamily: "jetbrains mono" }));
    let failed!: () => void;
    const load = vi.fn(() => new Promise<void>((_, reject) => (failed = () => reject(new Error("network")))));
    Object.defineProperty(document, "fonts", { configurable: true, value: { load, ready: Promise.resolve() } });
    let advance = 0;
    vi.spyOn(HTMLElement.prototype, "getBoundingClientRect").mockImplementation(
      () => ({ width: advance * 20 }) as DOMRect,
    );

    const { scroller } = renderLiveTerminal();
    advance = 8.7;
    await act(async () => failed());
    expect(scroller.style.getPropertyValue("--term-cell")).toBe("8.7px");
  });
});
