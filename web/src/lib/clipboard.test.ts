// @vitest-environment jsdom

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { armClipboardWrite } from "./clipboard";

class FakeClipboardItem {
  constructor(public readonly data: Record<string, Promise<Blob>>) {}
}

describe("armClipboardWrite", () => {
  let item: FakeClipboardItem | null;
  let write: ReturnType<typeof vi.fn>;
  let writeText: ReturnType<typeof vi.fn>;

  beforeEach(() => {
    item = null;
    write = vi.fn((items: FakeClipboardItem[]) => {
      item = items[0] ?? null;
      return Promise.resolve();
    });
    writeText = vi.fn().mockResolvedValue(undefined);
    Object.defineProperty(window, "isSecureContext", {
      configurable: true,
      value: true,
    });
    Object.defineProperty(navigator, "clipboard", {
      configurable: true,
      value: { write, writeText },
    });
    vi.stubGlobal("ClipboardItem", FakeClipboardItem);
  });

  afterEach(() => {
    vi.useRealTimers();
    vi.unstubAllGlobals();
  });

  it("starts a promise-valued ClipboardItem write during the gesture and resolves it later", async () => {
    const armed = armClipboardWrite();
    expect(write).toHaveBeenCalledTimes(1);
    expect(armed.resolve("copied through OSC 52")).toBe(true);

    const blob = await item!.data["text/plain"]!;
    expect(await blob.text()).toBe("copied through OSC 52");
    expect(writeText).not.toHaveBeenCalled();
  });

  it("rejects a late event after the arm times out", () => {
    vi.useFakeTimers();
    const armed = armClipboardWrite(500);
    vi.advanceTimersByTime(501);
    expect(armed.resolve("too late")).toBe(false);
  });

  it("rejects the pending ClipboardItem write when cancelled", async () => {
    const armed = armClipboardWrite();
    const pending = item!.data["text/plain"]!;

    armed.cancel();

    await expect(pending).rejects.toThrow("clipboard write cancelled");
    expect(armed.resolve("too late")).toBe(false);
  });

  it("falls back to writeText when ClipboardItem is unavailable", async () => {
    vi.stubGlobal("ClipboardItem", undefined);
    const armed = armClipboardWrite();
    expect(armed.resolve("fallback")).toBe(true);
    await vi.waitFor(() => expect(writeText).toHaveBeenCalledWith("fallback"));
  });
});
