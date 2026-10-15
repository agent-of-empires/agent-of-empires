// @vitest-environment jsdom
// Select-to-copy over panes that repaint under the selection; see useSelectionHold.

import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { act, screen } from "@testing-library/react";
import type { LiveFrame } from "../../hooks/useLiveTerminal";
import { installResizeObserver, linesFrame, renderLiveTerminal, stubElementSize } from "./liveTerminalHarness";

vi.mock("../../hooks/useWebSettings", () => ({
  useWebSettings: () => ({ settings: { mobileFontSize: 14, desktopFontSize: 14 }, update: vi.fn() }),
}));

stubElementSize({ clientWidth: () => 240, clientHeight: () => 600 });
const observers = installResizeObserver();
let outside: HTMLElement | null = null;

beforeEach(() => {
  vi.useFakeTimers();
  observers.clear();
});
afterEach(() => {
  vi.useRealTimers();
  document.getSelection()?.removeAllRanges();
  outside?.remove();
  outside = null;
});

/** A full-screen agent sliding its transcript through a fixed grid: every row changes each frame. */
const altFrame = (n: number) =>
  linesFrame([`line ${n}`, `line ${n + 1}`, `line ${n + 2}`, "", "> prompt"], { rows: 5, history: 0, altScreen: true });

const h = (i: number) => `h${String(i).padStart(3, "0")}`;
/** A 10-line history with `captured` lines of it in the window. */
const historyFrame = (captured: number) =>
  linesFrame([...Array.from({ length: captured }, (_, k) => h(10 - captured + 1 + k)), "s1", "s2", "s3"], {
    rows: 3,
    history: 10,
  });
/** The same pane with a full, capped scrollback after one append evicted the oldest line. */
const evictedFrame = () =>
  linesFrame([...Array.from({ length: 9 }, (_, k) => h(k + 2)), "s1", "s2", "s3", "s4"], { rows: 3, history: 10 });

function mount(frame: LiveFrame) {
  const view = renderLiveTerminal({ frame });
  observers.settle(200);
  const show = (next: LiveFrame, reading = false) => view.rerenderWith({ frame: next, reading });
  return { ...view, show, text: () => view.content().textContent ?? "" };
}

/** Selects a row's text with endpoints inside its text node, as a long-press word selection anchors. */
function selectRowText(text: string) {
  const node = screen.getByText(text).firstChild as Text;
  const range = document.createRange();
  range.setStart(node, 0);
  range.setEnd(node, node.data.length);
  const selection = document.getSelection()!;
  selection.removeAllRanges();
  selection.addRange(range);
  return selection;
}

it("holds the painted frame while a selection is live, then catches up", () => {
  const term = mount(altFrame(1));
  const selection = selectRowText("line 2");
  term.show(altFrame(2));
  expect(selection.toString()).toBe("line 2");
  expect(term.text()).toContain("line 1");
  selection.removeAllRanges();
  term.show(altFrame(3));
  expect(term.text()).toContain("line 5");
});

it("keeps painting when the selection is outside the terminal", () => {
  outside = document.createElement("p");
  outside.textContent = "elsewhere";
  document.body.append(outside);
  const term = mount(altFrame(1));
  const range = document.createRange();
  range.setStart(outside.firstChild!, 0);
  range.setEnd(outside.firstChild!, 9);
  document.getSelection()!.addRange(range);
  term.show(altFrame(2));
  expect(term.text()).toContain("line 4");
});

it("offers back-to-live while a frame is held, and releasing catches up", () => {
  const term = mount(altFrame(1));
  expect(screen.queryByLabelText("Back to live")).toBeNull();
  selectRowText("line 2");
  term.show(altFrame(2));
  act(() => screen.getByLabelText("Back to live").click());
  term.show(altFrame(3));
  expect(document.getSelection()?.isCollapsed ?? true).toBe(true);
  expect(term.text()).toContain("line 5");
});

it("lets uncaptured scrollback populate under a live selection", () => {
  // Dragging a selection up past the top widens the window; the wider frame must not be held out.
  const term = mount(historyFrame(1));
  const selection = selectRowText("s2");
  term.show(historyFrame(1), true);
  term.show(historyFrame(10), true);
  expect(term.text()).toContain("h001");
  expect(selection.toString()).toBe("s2");
});

it("holds newly exposed history once the selection can reach it", () => {
  // An eviction shifts the exposed prefix under unchanged row keys; re-deriving it would rewrite the selection.
  const term = mount(historyFrame(1));
  selectRowText("s2");
  term.show(historyFrame(1), true);
  term.show(historyFrame(10), true);
  const selection = selectRowText("h005");
  term.show(evictedFrame(), true);
  expect(selection.toString()).toBe("h005");
  expect(term.text()).toContain("h001");
});

it("holds through the pane's scrollback collapsing mid-selection", () => {
  // A cleared pane can never send the prefix the fold waits on; refolding must not loop.
  const deep = linesFrame(["h1999", "h2000", "s1", "s2", "s3"], { rows: 3, history: 2000 });
  const term = mount(deep);
  const selection = selectRowText("s2");
  term.show(deep, true);
  term.show(linesFrame(["s1", "s2", "s3"], { rows: 3, history: 0 }), true);
  expect(selection.toString()).toBe("s2");
});
