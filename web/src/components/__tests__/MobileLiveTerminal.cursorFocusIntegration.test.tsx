// @vitest-environment jsdom
//
// End-to-end within MobileLiveTerminal: focusing/blurring the hidden input
// must flip the rendered cursor cell's style, not just the parent's chrome
// ring (#2684). The cell has to survive being focused, blurred, and
// re-focused without drifting into a stuck state either way.

import { createRef } from "react";
import { describe, expect, it, vi, beforeAll } from "vitest";
import { fireEvent, render } from "@testing-library/react";
import { MobileLiveTerminal } from "../MobileLiveTerminal";
import type { LiveFrame } from "../../hooks/useLiveTerminal";

vi.mock("../../hooks/useWebSettings", () => ({
  useWebSettings: () => ({ settings: { mobileFontSize: 14, desktopFontSize: 14 }, update: vi.fn() }),
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
  cursor: { x: 2, y: 0 },
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
      uploadPastedImage={vi.fn()}
      forwardWheel={vi.fn()}
      forwardButton={vi.fn()}
      ctrlActiveRef={createRef<boolean>() as React.RefObject<boolean>}
      clearCtrl={vi.fn()}
      inputRef={inputRef}
      onInputFocusChange={vi.fn()}
      bottomAlign
    />,
  );
  const cursorCell = () => utils.container.querySelector("[data-live-cursor]") as HTMLElement | null;
  return { inputRef, cursorCell };
}

describe("MobileLiveTerminal cursor fill on focus", () => {
  it("starts hollow, fills and blinks on focus, reverts to hollow (no blink) on blur", () => {
    const { inputRef, cursorCell } = renderTerm();
    expect(cursorCell()!.style.backgroundColor).toBe("");
    expect(cursorCell()!.style.outline).toContain("var(--term-cursor");
    expect(cursorCell()!.className).toBe("");

    fireEvent.focus(inputRef.current!);
    expect(cursorCell()!.style.backgroundColor).toContain("var(--term-cursor");
    expect(cursorCell()!.style.outline).toBe("");
    expect(cursorCell()!.className).toContain("animate-term-cursor-blink");

    fireEvent.blur(inputRef.current!);
    expect(cursorCell()!.style.backgroundColor).toBe("");
    expect(cursorCell()!.style.outline).toContain("var(--term-cursor");
    expect(cursorCell()!.className).toBe("");
  });
});
