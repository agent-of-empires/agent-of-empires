// @vitest-environment jsdom
//
// Select-to-copy on a streaming pane depends on the row DOM nodes surviving
// the frames that arrive while the selection is being made: a remounted row
// collapses the browser selection, and on iOS also dismisses the Copy callout.

import { createRef } from "react";
import { beforeAll, expect, it, vi } from "vitest";
import { render, screen } from "@testing-library/react";
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

function frame(lines: string[], rows: number): LiveFrame {
  return {
    content: lines.join("\n") + "\n",
    lines,
    rows,
    history: Math.max(0, lines.length - rows),
    cursor: null,
    altScreen: false,
    mouse: false,
    mouseSgr: false,
  };
}

function terminal(f: LiveFrame) {
  return (
    <MobileLiveTerminal
      frame={f}
      connected
      active
      reading={false}
      sendResize={vi.fn()}
      setWindow={vi.fn()}
      setCadence={vi.fn()}
      enterReading={vi.fn()}
      returnToLive={vi.fn()}
      sendData={vi.fn()}
      typedWordRef={{ current: "" }}
      uploadPastedImage={vi.fn()}
      forwardWheel={vi.fn()}
      forwardButton={vi.fn()}
      ctrlActiveRef={createRef<boolean>() as React.RefObject<boolean>}
      clearCtrl={vi.fn()}
      inputRef={createRef<HTMLTextAreaElement>()}
      onInputFocusChange={vi.fn()}
      bottomAlign
      keyboardOpen={false}
    />
  );
}

it("keeps unchanged row nodes when the agent appends lines", () => {
  const { rerender } = render(terminal(frame(["alpha", "beta", "$ "], 3)));
  const alpha = screen.getByText("alpha").parentElement;
  const beta = screen.getByText("beta").parentElement;

  rerender(terminal(frame(["alpha", "beta", "gamma", "delta", "$ "], 3)));

  expect(screen.getByText("alpha").parentElement).toBe(alpha);
  expect(screen.getByText("beta").parentElement).toBe(beta);
  expect(screen.getByText("delta")).toBeTruthy();
});
