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
import { fireEvent, render } from "@testing-library/react";
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

function renderTerm() {
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
      forwardWheel={vi.fn()}
      forwardButton={vi.fn()}
      ctrlActiveRef={createRef<boolean>() as React.RefObject<boolean>}
      clearCtrl={vi.fn()}
      inputRef={inputRef}
      onInputFocusChange={vi.fn()}
      bottomAlign
    />,
  );
  return { input: inputRef.current!, sendData };
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

  it("still sends Ctrl+C as SIGINT (other chords unchanged)", () => {
    const { input, sendData } = renderTerm();
    fireEvent.keyDown(input, { key: "c", ctrlKey: true });
    expect(sendData).toHaveBeenCalledWith("\x03");
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
