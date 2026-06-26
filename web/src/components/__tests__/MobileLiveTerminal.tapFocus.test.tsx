// @vitest-environment jsdom
//
// Tapping anywhere on the terminal content focuses the hidden input, which is
// what brings up the soft keyboard on mobile (see #2243). The focus is
// synchronous inside the click handler for iOS, the active-element guard skips
// a redundant re-focus, and a live text selection is left alone so
// select-to-copy keeps working on desktop.

import { createRef } from "react";
import { describe, expect, it, vi, beforeAll } from "vitest";
import { fireEvent, render } from "@testing-library/react";
import { MobileLiveTerminal } from "../MobileLiveTerminal";
import type { LiveFrame } from "../../hooks/useLiveTerminal";

vi.mock("../../hooks/useWebSettings", () => ({
  useWebSettings: () => ({ settings: { mobileFontSize: 14 }, update: vi.fn() }),
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
  const utils = render(
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
      sendData={vi.fn()}
      forwardWheel={vi.fn()}
      forwardButton={vi.fn()}
      ctrlActiveRef={createRef<boolean>() as React.RefObject<boolean>}
      clearCtrl={vi.fn()}
      inputRef={inputRef}
      onInputFocusChange={vi.fn()}
      bottomAlign
    />,
  );
  const scroller = utils.container.querySelector("[data-live-terminal]")!.firstElementChild as HTMLElement;
  return { scroller, inputRef, utils };
}

describe("MobileLiveTerminal tap-to-focus", () => {
  it("focuses the hidden input when the terminal is tapped", () => {
    const { scroller, inputRef } = renderTerm();
    expect(document.activeElement).not.toBe(inputRef.current);
    fireEvent.click(scroller);
    expect(document.activeElement).toBe(inputRef.current);
  });

  it("does not steal focus from an active text selection", () => {
    const { scroller, inputRef } = renderTerm();
    const selection = window.getSelection();
    const range = document.createRange();
    range.selectNodeContents(scroller);
    selection?.removeAllRanges();
    selection?.addRange(range);
    expect(selection?.isCollapsed).toBe(false);
    fireEvent.click(scroller);
    expect(document.activeElement).not.toBe(inputRef.current);
  });
});
