// @vitest-environment jsdom
// A URL spread over several rows links as one target on every row it covers.

import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { installResizeObserver, linesFrame, renderLiveTerminal, stubElementSize } from "./liveTerminalHarness";

vi.mock("../../hooks/useWebSettings", () => ({
  useWebSettings: () => ({ settings: { mobileFontSize: 14, desktopFontSize: 14 }, update: vi.fn() }),
}));

stubElementSize({ clientWidth: () => 240, clientHeight: () => 600 });
const observers = installResizeObserver();

beforeEach(() => {
  vi.useFakeTimers();
  observers.clear();
});
afterEach(() => vi.useRealTimers());

const osc8 = (uri: string, text: string) => `\x1b]8;;${uri}\x1b\\${text}\x1b]8;;\x1b\\`;
const HREF = "https://example.com/abcd-efgh/ijkl";

it.each([
  ["a soft wrap the server flagged", ["see https://example.com/abcd", "-efgh/ijkl done"], [0]],
  [
    "a hard wrap carrying OSC 8 on each row",
    [`● ${osc8(HREF, "https://example.com/abcd")}`, `  ${osc8(HREF, "-efgh/ijkl")} thanks`],
    [],
  ],
])("links every row of %s to the whole URL", (_name, lines, wrapped) => {
  const view = renderLiveTerminal({ frame: linesFrame([...lines, "$ "], { rows: 3, wrapped }) });
  observers.settle();
  const anchors = [...view.container.querySelectorAll("a")];
  expect(anchors.map((a) => a.getAttribute("href"))).toEqual([HREF, HREF]);
  expect(anchors.map((a) => a.textContent).join("")).toBe(HREF);
});

it("keeps an unflagged row break, since a hyphen cannot say the URL goes on", () => {
  const view = renderLiveTerminal({
    frame: linesFrame(["see https://example.com/abcd", "-efgh/ijkl done", "$ "], { rows: 3 }),
  });
  observers.settle();
  expect([...view.container.querySelectorAll("a")].map((a) => a.getAttribute("href"))).toEqual([
    "https://example.com/abcd",
  ]);
});
