// @vitest-environment jsdom
//
// The agent surface bottom-aligns its screen chat-style (`mt-auto`) so a short
// screen's prompt sits just above the keyboard. The paired host/container
// shells are ordinary terminals and must top-align, so a near-empty bash prompt
// shows at the top of the pane like a normal terminal window rather than
// floating at the bottom. `bottomAlign` toggles that.

import { createRef } from "react";
import { describe, expect, it, vi, beforeAll } from "vitest";
import { render } from "@testing-library/react";
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

function renderTerm(bottomAlign: boolean) {
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
      inputRef={createRef<HTMLTextAreaElement>()}
      onInputFocusChange={vi.fn()}
      bottomAlign={bottomAlign}
    />,
  );
  return utils.container.querySelector("[data-live-content]") as HTMLElement;
}

describe("MobileLiveTerminal screen alignment", () => {
  it("bottom-aligns the agent surface (mt-auto)", () => {
    expect(renderTerm(true).className).toContain("mt-auto");
  });

  it("top-aligns a paired shell like a normal terminal (no mt-auto)", () => {
    expect(renderTerm(false).className).not.toContain("mt-auto");
  });
});
