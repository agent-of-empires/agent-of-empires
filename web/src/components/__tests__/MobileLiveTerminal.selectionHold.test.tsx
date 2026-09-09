// @vitest-environment jsdom
//
// Select-to-copy over a full-screen agent, which repaints every row of every
// frame. See useSelectionHold for why the paint has to stop.

import { createRef } from "react";
import { afterEach, beforeAll, beforeEach, expect, it, vi } from "vitest";
import { act, render, screen } from "@testing-library/react";
import { MobileLiveTerminal } from "../MobileLiveTerminal";
import type { LiveFrame } from "../../hooks/useLiveTerminal";

vi.mock("../../hooks/useWebSettings", () => ({
  useWebSettings: () => ({ settings: { mobileFontSize: 14, desktopFontSize: 14 }, update: vi.fn() }),
}));

const WIDTH = 240;
let roCallbacks: Array<() => void> = [];

beforeAll(() => {
  globalThis.ResizeObserver = class {
    private cb: () => void;
    constructor(cb: () => void) {
      this.cb = cb;
    }
    observe() {
      roCallbacks.push(this.cb);
    }
    unobserve() {}
    disconnect() {
      roCallbacks = roCallbacks.filter((c) => c !== this.cb);
    }
  } as unknown as typeof ResizeObserver;
  Object.defineProperty(HTMLElement.prototype, "clientWidth", { configurable: true, get: () => WIDTH });
  Object.defineProperty(HTMLElement.prototype, "clientHeight", { configurable: true, get: () => 600 });
});

beforeEach(() => {
  vi.useFakeTimers();
  roCallbacks = [];
});

afterEach(() => {
  vi.useRealTimers();
  document.getSelection()?.removeAllRanges();
  outside?.remove();
  outside = null;
});

let outside: HTMLElement | null = null;

// A full-screen agent: no scrollback, and the transcript slides up through a
// fixed grid, so every screen row holds new text on the next frame.
function altFrame(n: number): LiveFrame {
  const lines = [`line ${n}`, `line ${n + 1}`, `line ${n + 2}`, "", "> prompt"];
  return {
    content: lines.join("\n") + "\n",
    lines,
    rows: 5,
    history: 0,
    cursor: null,
    altScreen: true,
    mouse: false,
    mouseSgr: false,
  };
}

// A normal-buffer pane whose capture window is far smaller than its history:
// `captured` screen rows plus one line of scrollback at first, then the full
// history once reading widens the window.
function historyFrame(captured: number): LiveFrame {
  const rows = 3;
  const history = 10;
  const lines = [];
  for (let i = history - captured + 1; i <= history; i++) lines.push(`h${String(i).padStart(3, "0")}`);
  lines.push("s1", "s2", "s3");
  return {
    content: lines.join("\n") + "\n",
    lines,
    rows,
    history,
    cursor: null,
    altScreen: false,
    mouse: false,
    mouseSgr: false,
  };
}

function terminal(f: LiveFrame, reading = false) {
  return (
    <MobileLiveTerminal
      frame={f}
      connected
      active
      reading={reading}
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

/** Select a row's whole text with both endpoints INSIDE its text node, the
 *  way a long-press word selection anchors. Selecting the node's contents
 *  from the row element instead would leave the range endpoints outside the
 *  rewritten data and survive a repaint that a real gesture does not. */
function selectRowText(text: string) {
  const row = screen.getByText(text);
  const node = row.firstChild as Text;
  const range = document.createRange();
  range.setStart(node, 0);
  range.setEnd(node, node.data.length);
  const selection = document.getSelection()!;
  selection.removeAllRanges();
  selection.addRange(range);
  return selection;
}

function mount(f: LiveFrame) {
  const result = render(terminal(f));
  act(() => {
    for (const cb of roCallbacks) cb();
    vi.advanceTimersByTime(200);
  });
  return result;
}

it("holds the painted frame while a selection is live, then catches up", () => {
  const { container, rerender } = mount(altFrame(1));
  const selection = selectRowText("line 2");

  rerender(terminal(altFrame(2)));

  expect(selection.toString()).toBe("line 2");
  expect(container.querySelector("[data-live-content]")?.textContent).toContain("line 1");

  selection.removeAllRanges();
  rerender(terminal(altFrame(3)));

  expect(container.querySelector("[data-live-content]")?.textContent).toContain("line 5");
});

it("keeps painting when the selection is outside the terminal", () => {
  outside = document.createElement("p");
  outside.textContent = "elsewhere";
  document.body.append(outside);
  const { container, rerender } = mount(altFrame(1));
  const range = document.createRange();
  range.setStart(outside.firstChild!, 0);
  range.setEnd(outside.firstChild!, 9);
  document.getSelection()!.addRange(range);

  rerender(terminal(altFrame(2)));

  expect(container.querySelector("[data-live-content]")?.textContent).toContain("line 4");
});

it("offers the back-to-live control while a frame is held, and releasing catches up", () => {
  const { container, rerender } = mount(altFrame(1));
  expect(screen.queryByLabelText("Back to live")).toBeNull();

  selectRowText("line 2");
  rerender(terminal(altFrame(2)));
  const backToLive = screen.getByLabelText("Back to live");

  act(() => backToLive.click());
  rerender(terminal(altFrame(3)));

  expect(document.getSelection()?.isCollapsed ?? true).toBe(true);
  expect(container.querySelector("[data-live-content]")?.textContent).toContain("line 5");
});

it("lets uncaptured scrollback populate under a live selection", () => {
  // Selecting at the live edge and dragging upward past the top scrolls into
  // scrollback (enterReading widens the capture window). The hold must not
  // swallow that wider frame, or the drag extends into the blank spacer
  // instead of the older text it just asked for.
  const { container, rerender } = mount(historyFrame(1));
  const selection = selectRowText("s2");

  // Scrolling up flips the pane into reading mode first; the wider capture
  // window lands a frame later.
  rerender(terminal(historyFrame(1), true));
  rerender(terminal(historyFrame(10), true));

  const text = container.querySelector("[data-live-content]")?.textContent ?? "";
  expect(text).toContain("h001");
  expect(selection.toString()).toBe("s2");
});
