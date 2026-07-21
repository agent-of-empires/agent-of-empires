// @vitest-environment jsdom
//
// Clipboard chords in the live terminal on a physical keyboard (#2384). On
// Linux/Windows the paste shortcut is Ctrl+V; the Ctrl+letter chord handler
// used to swallow it into a literal ^V to tmux AND preventDefault the keydown,
// which blocked the browser's paste event from ever firing. Ctrl+V must now
// fall through so the native paste event reaches onPaste (bracketed paste).
// Ctrl+Shift+C copies the rendered terminal selection (read explicitly because
// the hidden input is focused), while plain Ctrl+C stays SIGINT and every
// other Ctrl+letter chord keeps working.

import { createRef } from "react";
import { describe, expect, it, vi, beforeAll } from "vitest";
import { fireEvent, render, waitFor } from "@testing-library/react";
import { MobileLiveTerminal } from "../MobileLiveTerminal";
import type { LiveFrame } from "../../hooks/useLiveTerminal";

vi.mock("../../hooks/useWebSettings", () => ({
  useWebSettings: () => ({ settings: { mobileFontSize: 14, desktopFontSize: 14 }, update: vi.fn() }),
}));

const writeClipboard = vi.fn();
vi.mock("../../lib/clipboard", () => ({
  writeClipboard: (text: string) => writeClipboard(text),
}));

beforeAll(() => {
  globalThis.ResizeObserver = class {
    observe() {}
    unobserve() {}
    disconnect() {}
  } as unknown as typeof ResizeObserver;
});

const frame: LiveFrame = {
  content: "$ \n",
  rows: 3,
  history: 1000,
  cursor: null,
  altScreen: false,
  mouse: false,
  mouseSgr: false,
};

function renderTerm(uploadPastedImage = vi.fn().mockResolvedValue(null)) {
  const inputRef = createRef<HTMLTextAreaElement>();
  const sendData = vi.fn();
  render(
    <MobileLiveTerminal
      frame={frame}
      connected
      active
      reading={false}
      sendResize={vi.fn()}
      setWindow={vi.fn()}
      setCadence={vi.fn()}
      enterReading={vi.fn()}
      returnToLive={vi.fn()}
      sendData={sendData}
      uploadPastedImage={uploadPastedImage}
      forwardWheel={vi.fn()}
      forwardButton={vi.fn()}
      ctrlActiveRef={createRef<boolean>() as React.RefObject<boolean>}
      clearCtrl={vi.fn()}
      inputRef={inputRef}
      onInputFocusChange={vi.fn()}
      bottomAlign
    />,
  );
  return { input: inputRef.current!, sendData, uploadPastedImage };
}

// A clipboard item wrapping a File, as clipboardData.items exposes it.
function imageItem(file: File): DataTransferItem {
  return {
    kind: "file",
    type: file.type,
    getAsFile: () => file,
  } as unknown as DataTransferItem;
}

function stubKeyboardLayout(entries: [string, string][]) {
  const original = Object.getOwnPropertyDescriptor(navigator, "keyboard");
  const getLayoutMap = vi.fn().mockResolvedValue(new Map(entries));
  Object.defineProperty(navigator, "keyboard", {
    configurable: true,
    value: { getLayoutMap },
  });
  return {
    getLayoutMap,
    restore: () => {
      if (original) {
        Object.defineProperty(navigator, "keyboard", original);
      } else {
        delete (navigator as Navigator & { keyboard?: unknown }).keyboard;
      }
    },
  };
}

describe("MobileLiveTerminal paste", () => {
  it("does not swallow Ctrl+V into a literal ^V, and the paste event sends a bracketed paste", () => {
    const { input, sendData } = renderTerm();

    // Ctrl+V keydown must NOT be intercepted: no literal ^V (\x16) to tmux,
    // and the default action is left intact so the paste event can fire.
    const keydown = fireEvent.keyDown(input, { key: "v", ctrlKey: true });
    expect(keydown).toBe(true); // not preventDefault'd
    expect(sendData).not.toHaveBeenCalledWith("\x16");

    // The native paste event onPaste handles it as a bracketed paste.
    fireEvent.paste(input, {
      clipboardData: { getData: (t: string) => (t === "text/plain" ? "hello world" : "") },
    });
    expect(sendData).toHaveBeenCalledWith("\x1b[200~hello world\x1b[201~");
  });

  it("uploads a pasted image and bracketed-pastes the returned host path (#2678)", async () => {
    const upload = vi.fn().mockResolvedValue("/repo/.aoe-pasted-images/aoe-paste-x.png");
    const { input, sendData } = renderTerm(upload);

    const file = new File([new Uint8Array([1, 2, 3])], "shot.png", { type: "image/png" });
    fireEvent.paste(input, {
      clipboardData: { getData: () => "", items: [imageItem(file)] },
    });

    expect(upload).toHaveBeenCalledWith(file);
    // Path resolves on a microtask; flush before asserting the send.
    await vi.waitFor(() =>
      expect(sendData).toHaveBeenCalledWith("\x1b[200~ /repo/.aoe-pasted-images/aoe-paste-x.png \x1b[201~"),
    );
  });

  it("escapes spaces in the pasted path so a dir like 'Agent of Empires' stays one token", async () => {
    const upload = vi.fn().mockResolvedValue("/Users/me/Agent of Empires/.aoe-pasted-images/x.png");
    const { input, sendData } = renderTerm(upload);

    const file = new File([new Uint8Array([1])], "s.png", { type: "image/png" });
    fireEvent.paste(input, { clipboardData: { getData: () => "", items: [imageItem(file)] } });

    await vi.waitFor(() =>
      expect(sendData).toHaveBeenCalledWith(
        "\x1b[200~ /Users/me/Agent\\ of\\ Empires/.aoe-pasted-images/x.png \x1b[201~",
      ),
    );
  });

  it("keeps clipboard text alongside a pasted image", async () => {
    const upload = vi.fn().mockResolvedValue("/repo/.aoe-pasted-images/x.png");
    const { input, sendData } = renderTerm(upload);

    const file = new File([new Uint8Array([1])], "s.png", { type: "image/png" });
    fireEvent.paste(input, {
      clipboardData: { getData: (t: string) => (t === "text/plain" ? "look at" : ""), items: [imageItem(file)] },
    });

    await vi.waitFor(() =>
      expect(sendData).toHaveBeenCalledWith("\x1b[200~ look at /repo/.aoe-pasted-images/x.png \x1b[201~"),
    );
  });

  it("a failed image upload sends nothing (no crash, no partial paste)", async () => {
    const upload = vi.fn().mockResolvedValue(null);
    const { input, sendData } = renderTerm(upload);

    const file = new File([new Uint8Array([1])], "s.png", { type: "image/png" });
    fireEvent.paste(input, { clipboardData: { getData: () => "", items: [imageItem(file)] } });

    await vi.waitFor(() => expect(upload).toHaveBeenCalled());
    expect(sendData).not.toHaveBeenCalled();
  });

  it("still sends Ctrl+C as SIGINT (other chords unchanged)", () => {
    const { input, sendData } = renderTerm();
    fireEvent.keyDown(input, { key: "c", ctrlKey: true });
    expect(sendData).toHaveBeenCalledWith("\x03");
  });

  it("sends plain Enter as carriage return", () => {
    const { input, sendData } = renderTerm();

    expect(fireEvent.keyDown(input, { key: "Enter" })).toBe(false);
    expect(sendData).toHaveBeenCalledWith("\r");
  });

  it("sends Ctrl+Enter as terminal Meta Enter for agent line breaks", () => {
    const { input, sendData } = renderTerm();

    expect(fireEvent.keyDown(input, { key: "Enter", ctrlKey: true })).toBe(false);
    expect(sendData).toHaveBeenCalledWith("\x1b\r");
  });

  it("sends Shift+Enter as terminal Meta Enter for agent line breaks", () => {
    const { input, sendData } = renderTerm();

    expect(fireEvent.keyDown(input, { key: "Enter", shiftKey: true })).toBe(false);
    expect(sendData).toHaveBeenCalledWith("\x1b\r");
  });

  it("sends Ctrl+Shift+Enter as terminal Meta Enter for agent line breaks", () => {
    const { input, sendData } = renderTerm();

    expect(fireEvent.keyDown(input, { key: "Enter", ctrlKey: true, shiftKey: true })).toBe(false);
    expect(sendData).toHaveBeenCalledWith("\x1b\r");
  });

  it("sends Alt+Enter as carriage return", () => {
    const { input, sendData } = renderTerm();

    expect(fireEvent.keyDown(input, { key: "Enter", altKey: true })).toBe(false);
    expect(sendData).toHaveBeenCalledWith("\r");
  });

  it("forwards Alt+letter chords as terminal Meta sequences", () => {
    const { input, sendData } = renderTerm();

    expect(fireEvent.keyDown(input, { key: "v", code: "KeyV", altKey: true })).toBe(false);
    expect(sendData).toHaveBeenCalledWith("\x1bv");

    expect(fireEvent.keyDown(input, { key: "V", code: "KeyV", altKey: true, shiftKey: true })).toBe(false);
    expect(sendData).toHaveBeenCalledWith("\x1bV");
  });

  it("uses KeyboardEvent.code for Alt+letter when the browser key is a symbol", () => {
    const { input, sendData } = renderTerm();

    expect(fireEvent.keyDown(input, { key: "√", code: "KeyV", altKey: true })).toBe(false);
    expect(sendData).toHaveBeenCalledWith("\x1bv");
  });

  it("uses Keyboard Layout Map before physical-code fallback when available", async () => {
    const layout = stubKeyboardLayout([["KeyQ", "a"]]);
    try {
      const { input, sendData } = renderTerm();
      await waitFor(() => expect(layout.getLayoutMap).toHaveBeenCalled());

      expect(fireEvent.keyDown(input, { key: "æ", code: "KeyQ", altKey: true })).toBe(false);
      expect(sendData).toHaveBeenCalledWith("\x1ba");
    } finally {
      layout.restore();
    }
  });

  it("does not convert macOS dead keys into Meta letters", () => {
    const { input, sendData } = renderTerm();

    expect(fireEvent.keyDown(input, { key: "Dead", code: "KeyE", altKey: true })).toBe(true);
    expect(sendData).not.toHaveBeenCalled();
  });

  it("does not send Alt+letter while IME composition is active", () => {
    const { input, sendData } = renderTerm();

    fireEvent.compositionStart(input);
    expect(fireEvent.keyDown(input, { key: "v", code: "KeyV", altKey: true })).toBe(true);
    expect(sendData).not.toHaveBeenCalled();
  });

  it("encodes Backspace modifier chords", () => {
    const { input, sendData } = renderTerm();

    expect(fireEvent.keyDown(input, { key: "Backspace", altKey: true })).toBe(false);
    expect(sendData).toHaveBeenCalledWith("\x1b\x7f");

    expect(fireEvent.keyDown(input, { key: "Backspace", ctrlKey: true })).toBe(false);
    expect(sendData).toHaveBeenCalledWith("\x7f");
  });

  it("encodes unmodified navigation and edit keys", () => {
    const { input, sendData } = renderTerm();

    for (const { key, expected } of [
      { key: "ArrowUp", expected: "\x1b[A" },
      { key: "ArrowDown", expected: "\x1b[B" },
      { key: "ArrowRight", expected: "\x1b[C" },
      { key: "ArrowLeft", expected: "\x1b[D" },
      { key: "Insert", expected: "\x1b[2~" },
      { key: "Delete", expected: "\x1b[3~" },
      { key: "Home", expected: "\x1b[H" },
      { key: "End", expected: "\x1b[F" },
      { key: "PageUp", expected: "\x1b[5~" },
      { key: "PageDown", expected: "\x1b[6~" },
    ]) {
      sendData.mockClear();
      expect(fireEvent.keyDown(input, { key })).toBe(false);
      expect(sendData).toHaveBeenCalledWith(expected);
    }
  });

  it("encodes modified navigation and edit keys with xterm CSI modifier forms", () => {
    const { input, sendData } = renderTerm();

    for (const { key, init, expected } of [
      { key: "ArrowUp", init: { shiftKey: true }, expected: "\x1b[1;2A" },
      { key: "ArrowDown", init: { altKey: true }, expected: "\x1b[1;3B" },
      { key: "ArrowRight", init: { altKey: true }, expected: "\x1b[1;3C" },
      { key: "ArrowLeft", init: { ctrlKey: true }, expected: "\x1b[1;5D" },
      { key: "Home", init: { ctrlKey: true }, expected: "\x1b[1;5H" },
      { key: "End", init: { ctrlKey: true, shiftKey: true }, expected: "\x1b[1;6F" },
      { key: "Insert", init: { shiftKey: true }, expected: "\x1b[2;2~" },
      { key: "PageUp", init: { altKey: true }, expected: "\x1b[5;3~" },
      { key: "PageDown", init: { altKey: true }, expected: "\x1b[6;3~" },
      { key: "Delete", init: { ctrlKey: true }, expected: "\x1b[3;5~" },
    ]) {
      sendData.mockClear();
      expect(fireEvent.keyDown(input, { key, ...init })).toBe(false);
      expect(sendData).toHaveBeenCalledWith(expected);
    }
  });

  it("leaves Meta navigation to the browser", () => {
    const { input, sendData } = renderTerm();

    expect(fireEvent.keyDown(input, { key: "ArrowLeft", metaKey: true })).toBe(true);
    expect(sendData).not.toHaveBeenCalled();
  });

  it("encodes Tab and Escape keys", () => {
    const { input, sendData } = renderTerm();

    expect(fireEvent.keyDown(input, { key: "Tab" })).toBe(false);
    expect(sendData).toHaveBeenCalledWith("\t");

    sendData.mockClear();
    expect(fireEvent.keyDown(input, { key: "Tab", shiftKey: true })).toBe(false);
    expect(sendData).toHaveBeenCalledWith("\x1b[Z");

    sendData.mockClear();
    expect(fireEvent.keyDown(input, { key: "Escape" })).toBe(false);
    expect(sendData).toHaveBeenCalledWith("\x1b");
  });

  it("leaves Ctrl+Alt printable chords alone for AltGr-style input", () => {
    const { input, sendData } = renderTerm();

    expect(fireEvent.keyDown(input, { key: "v", ctrlKey: true, altKey: true })).toBe(true);
    expect(sendData).not.toHaveBeenCalled();
  });

  it("copies the terminal selection on Ctrl+Shift+C without sending a control code", () => {
    writeClipboard.mockClear();
    const selSpy = vi.spyOn(window, "getSelection").mockReturnValue({
      toString: () => "selected output",
    } as unknown as Selection);
    try {
      const { input, sendData } = renderTerm();
      fireEvent.keyDown(input, { key: "C", ctrlKey: true, shiftKey: true });
      expect(writeClipboard).toHaveBeenCalledWith("selected output");
      // Must NOT also send ^C (SIGINT) to tmux.
      expect(sendData).not.toHaveBeenCalledWith("\x03");
    } finally {
      selSpy.mockRestore();
    }
  });

  it("Ctrl+Shift+C with no selection is a no-op (no copy, no control code)", () => {
    writeClipboard.mockClear();
    const selSpy = vi.spyOn(window, "getSelection").mockReturnValue({
      toString: () => "",
    } as unknown as Selection);
    try {
      const { input, sendData } = renderTerm();
      fireEvent.keyDown(input, { key: "C", ctrlKey: true, shiftKey: true });
      expect(writeClipboard).not.toHaveBeenCalled();
      expect(sendData).not.toHaveBeenCalledWith("\x03");
    } finally {
      selSpy.mockRestore();
    }
  });
});
