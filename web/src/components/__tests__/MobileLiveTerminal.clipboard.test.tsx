// @vitest-environment jsdom

import { createRef } from "react";
import { fireEvent, render } from "@testing-library/react";
import { beforeAll, describe, expect, it, vi } from "vitest";
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
  content: "copy me\n",
  rows: 3,
  history: 0,
  cursor: null,
  altScreen: true,
  mouse: true,
  mouseSgr: true,
};

describe("MobileLiveTerminal OSC 52 clipboard", () => {
  it("arms the parent clipboard bridge on a forwarded left-button release", () => {
    const armAgentClipboard = vi.fn();
    const view = render(
      <MobileLiveTerminal
        frame={frame}
        armAgentClipboard={armAgentClipboard}
        connected
        active
        reading={false}
        sendResize={vi.fn()}
        setWindow={vi.fn()}
        setCadence={vi.fn()}
        enterReading={vi.fn()}
        returnToLive={vi.fn()}
        sendData={vi.fn()}
        uploadPastedImage={vi.fn().mockResolvedValue(null)}
        forwardWheel={vi.fn()}
        forwardButton={vi.fn()}
        ctrlActiveRef={createRef<boolean>() as React.RefObject<boolean>}
        clearCtrl={vi.fn()}
        inputRef={createRef<HTMLTextAreaElement>()}
        onInputFocusChange={vi.fn()}
        bottomAlign
        keyboardOpen={false}
      />,
    );
    const scroller = view.container.querySelector("[data-live-terminal] > div") as HTMLElement;

    fireEvent.pointerDown(scroller, {
      pointerType: "mouse",
      pointerId: 1,
      button: 0,
      clientX: 10,
      clientY: 10,
    });
    fireEvent.pointerUp(scroller, {
      pointerType: "mouse",
      pointerId: 1,
      button: 0,
      clientX: 80,
      clientY: 10,
    });

    expect(armAgentClipboard).toHaveBeenCalledTimes(1);
  });
});
