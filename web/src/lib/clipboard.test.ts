// @vitest-environment jsdom

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { armClipboardWrite, readClipboardText } from "./clipboard";

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

describe("readClipboardText", () => {
  afterEach(() => {
    delete (navigator as { clipboard?: unknown }).clipboard;
  });

  it("normalises whichever text type the source app wrote", async () => {
    Object.defineProperty(window, "isSecureContext", { configurable: true, value: true });
    const cases: Array<[string, string, string]> = [
      ["text/plain", "plain\ntext", "plain\ntext"],
      [
        "text/uri-list",
        "# comment\r\nhttps://a.example\r\nhttps://b.example\r\n",
        "https://a.example\nhttps://b.example",
      ],
      ["text/html", '<p><a href="https://x.example/pr/1">PR</a></p>', "https://x.example/pr/1"],
      ["text/html", '<a href="">click here</a>', "click here"],
      ["text/html", "<p> just text </p>", "just text"],
    ];
    for (const [type, raw, expected] of cases) {
      const item = { types: [type], getType: async () => new Blob([raw], { type }) };
      Object.defineProperty(navigator, "clipboard", { configurable: true, value: { read: async () => [item] } });
      expect(await readClipboardText(), `${type}: ${raw}`).toBe(expected);
    }
  });

  it("returns empty text outside a secure context or when the read is refused", async () => {
    Object.defineProperty(navigator, "clipboard", {
      configurable: true,
      value: { read: async () => Promise.reject(new Error("denied")) },
    });
    Object.defineProperty(window, "isSecureContext", { configurable: true, value: true });
    expect(await readClipboardText()).toBe("");
    Object.defineProperty(window, "isSecureContext", { configurable: true, value: false });
    expect(await readClipboardText()).toBe("");
  });
});
