// @vitest-environment jsdom
// Row nodes must survive streamed frames, or a selection in progress collapses; the trimmed row count must
// also hold while an agent's bottom row oscillates (#2087).

import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { act, screen } from "@testing-library/react";
import { CHAR_W, installResizeObserver, linesFrame, renderLiveTerminal, stubElementSize } from "./liveTerminalHarness";

vi.mock("../../hooks/useWebSettings", () => ({
  useWebSettings: () => ({ settings: { mobileFontSize: 14, desktopFontSize: 14 }, update: vi.fn() }),
}));

const WIDTH = 240;
const SHRINK_DELAY_MS = 1500;
stubElementSize({ clientWidth: () => WIDTH, clientHeight: () => 600 });
const observers = installResizeObserver();

beforeEach(() => {
  vi.useFakeTimers();
  observers.clear();
});
afterEach(() => vi.useRealTimers());

const rowOf = (text: string) => screen.getByText(text).parentElement;

it("keeps unchanged row nodes when the agent appends lines", () => {
  const view = renderLiveTerminal({ frame: linesFrame(["alpha", "beta", "$ "], { rows: 3, history: 0 }) });
  const [alpha, beta] = [rowOf("alpha"), rowOf("beta")];
  view.rerenderWith({ frame: linesFrame(["alpha", "beta", "gamma", "delta", "$ "], { rows: 3, history: 2 }) });
  expect(rowOf("alpha")).toBe(alpha);
  expect(rowOf("beta")).toBe(beta);
  expect(screen.getByText("delta")).toBeTruthy();
});

it("keeps row nodes when the window slides past wrapped lines", () => {
  // Two lines wider than the 28-column pane scroll off the top while history grows to match.
  const wide = (tag: string) => `${tag} `.repeat(20).trimEnd();
  const view = renderLiveTerminal({
    frame: linesFrame([wide("one"), wide("two"), "alpha", "beta", "$ "], { rows: 3, history: 2 }),
  });
  observers.settle(150);
  expect(Math.floor(WIDTH / CHAR_W)).toBe(28);
  expect(view.rowCount()).toBeGreaterThan(5);
  const alpha = rowOf("alpha");
  view.rerenderWith({ frame: linesFrame(["alpha", "beta", "gamma", "delta", "$ "], { rows: 3, history: 4 }) });
  expect(rowOf("alpha")).toBe(alpha);
});

it("holds the trimmed row count while the bottom row oscillates, then trims once quiet", () => {
  const body = ["HEADER", ...Array.from({ length: 16 }, (_, i) => `body ${i + 1}`), "INPUTBOX>", ""];
  const rows = body.length + 1;
  const spinnerOn = linesFrame([...body, "spinner working..."], { rows, history: 0 });
  const spinnerOff = linesFrame([...body, ""], { rows, history: 0 });
  const view = renderLiveTerminal({ frame: spinnerOn });
  expect(view.rowCount()).toBe(rows);

  // Longer than the shrink delay, so a shrink firing between redraws would show.
  for (let elapsed = 0; elapsed < 2 * SHRINK_DELAY_MS; elapsed += 240) {
    for (const frame of [spinnerOff, spinnerOn]) {
      view.rerenderWith({ frame });
      act(() => vi.advanceTimersByTime(120));
      expect(view.rowCount()).toBe(rows);
    }
  }
  view.rerenderWith({ frame: spinnerOff });
  act(() => vi.advanceTimersByTime(SHRINK_DELAY_MS));
  expect(view.rowCount()).toBe(body.indexOf("INPUTBOX>") + 1);
});
