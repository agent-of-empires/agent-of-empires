import type { Page } from "@playwright/test";
import { test, expect } from "./helpers/mockedTest";
import { makeLiveFrame, mockTerminalApis } from "./helpers/terminal-mocks";
import { openLiveSession, scroller } from "./helpers/liveTerminal";

// #4204: a reader scrolled into history must keep their line in view while the rows above it change. Reading
// mode loads the full history into a spacer that reserves one row per pane line, though a line can wrap to
// several; once scrollback is full, its oldest lines are trimmed in a chunk; and a reading window deeper than
// the VT grid's scrollback is served by tmux capture, which numbers lines from an older origin and encodes
// styles differently.
const HISTORY = 400;
const TRIM = 200;
const TMUX_EXTRA = 600;
/** The VT grid ends every row with a reset. */
const vtRow = (line: (i: number) => string) => (i: number) => `${line(i)}\x1b[0m`;
const lineKinds = [
  { name: "lines that fit the grid", line: (i: number) => `L${String(i).padStart(4, "0")} short` },
  { name: "lines that wrap", line: (i: number) => `L${String(i).padStart(4, "0")} ${"x".repeat(400)}` },
];
const scenarios = [
  "history loads above the reader",
  "the oldest scrollback is trimmed",
  "reading switches from the VT grid to tmux",
] as const;
const cases = scenarios.flatMap((scenario) => lineKinds.map((kind) => ({ scenario, ...kind })));

/** The first history line whose first row is in view: always loaded text, never the spacer above it. */
async function firstVisibleLine(page: Page) {
  return page.evaluate(() => {
    const content = document.querySelector("[data-live-content]")!;
    const top = content.closest("[data-live-terminal] > div")!.getBoundingClientRect().top;
    for (const row of content.querySelectorAll(":scope > *")) {
      const id = /^L(\d{4}) /.exec(row.textContent ?? "")?.[1];
      const rect = row.getBoundingClientRect();
      if (id && rect.top >= top) return id;
    }
    return null;
  });
}

/** Offset of `L<id>`'s first row from the scroller's top edge, or null when that row is not rendered. */
async function lineOffset(page: Page, id: string) {
  return page.evaluate((id) => {
    const content = document.querySelector("[data-live-content]")!;
    const top = content.closest("[data-live-terminal] > div")!.getBoundingClientRect().top;
    for (const row of content.querySelectorAll(":scope > *")) {
      if ((row.textContent ?? "").startsWith(`L${id} `)) return row.getBoundingClientRect().top - top;
    }
    return null;
  }, id);
}

/** The history line whose text is at the top of the view, counting a wrapped line's later rows as that line. */
async function topLine(page: Page) {
  return page.evaluate(() => {
    const content = document.querySelector("[data-live-content]")!;
    const top = content.closest("[data-live-terminal] > div")!.getBoundingClientRect().top;
    let id: string | null = null;
    for (const row of content.querySelectorAll(":scope > *")) {
      id = /^L(\d{4}) /.exec(row.textContent ?? "")?.[1] ?? id;
      if (row.getBoundingClientRect().bottom > top) return id;
    }
    return null;
  });
}

/** Open a session of HISTORY lines, take over its frames, and scroll up until reading mode requests history. */
async function scrollIntoHistory(page: Page, line: (i: number) => string) {
  const handle = await openLiveSession(
    page,
    await mockTerminalApis(page, { liveHistory: HISTORY, liveHistoryLine: vtRow(line) }),
  );
  const debug = page.locator("[data-live-debug]");
  const field = async (key: string) =>
    Number(new RegExp(`${key}=([\\d.]+)`).exec((await debug.textContent()) ?? "")?.[1]);
  const rows = await field("rows");
  const lineH = await field("lineH");

  // Take over the frames, re-sending the live window unchanged, so the history request below waits for us.
  await handle.pushLiveFrame(makeLiveFrame({ rows, history: HISTORY, window: rows * 2, line: vtRow(line) }));

  const el = scroller(page);
  const startTop = await el.evaluate((n) => n.scrollTop);
  await el.hover();
  await page.mouse.wheel(0, -lineH * 8);
  await expect.poll(() => el.evaluate((n) => n.scrollTop)).toBeLessThan(startTop - lineH * 2);
  const fullWindow = rows + HISTORY;
  const requested = () =>
    handle.liveMessages.some((m) => {
      try {
        const msg = JSON.parse(m.toString()) as { type?: string; lines?: number };
        return msg.type === "window" && msg.lines === fullWindow;
      } catch {
        return false;
      }
    });
  await expect.poll(requested, { message: "scrolling up must enter reading mode and request history" }).toBe(true);
  return { handle, field, rows, lineH, fullWindow };
}

test.describe("Live terminal scroll anchor", () => {
  test.use({ viewport: { width: 1280, height: 800 }, hasTouch: false });

  for (const { scenario, name, line } of cases) {
    test(`${scenario} without moving the reader's line: ${name}`, async ({ page }) => {
      const { handle, field, rows, lineH, fullWindow } = await scrollIntoHistory(page, line);
      const fullFrame = makeLiveFrame({ rows, history: HISTORY, window: fullWindow, line: vtRow(line) });
      if (scenario === "the oldest scrollback is trimmed") {
        await handle.pushLiveFrame(fullFrame);
        await expect.poll(() => field("spacer")).toBe(0);
      }
      const ref = await firstVisibleLine(page);
      expect(ref, "a history line must be in view after scrolling up").not.toBeNull();
      const before = (await lineOffset(page, ref!))!;

      if (scenario === "history loads above the reader") {
        await handle.pushLiveFrame(fullFrame);
        await expect.poll(() => field("spacer")).toBe(0);
      } else if (scenario === "reading switches from the VT grid to tmux") {
        // tmux holds TMUX_EXTRA older lines, so the same window starts that many lines later in its numbering.
        const history = HISTORY + TMUX_EXTRA;
        const tmuxLine = (i: number) =>
          i > TMUX_EXTRA ? line(i - TMUX_EXTRA) : `O${String(i).padStart(4, "0")} older`;
        await handle.pushLiveFrame(makeLiveFrame({ rows, history, window: fullWindow, line: tmuxLine }));
        await expect.poll(() => field("hist")).toBe(history);
      } else {
        // The oldest TRIM lines go and one new line arrives, so every remaining line moves up TRIM places.
        const history = HISTORY - TRIM + 1;
        await handle.pushLiveFrame(
          makeLiveFrame({ rows, history, window: rows + history, line: (i) => vtRow(line)(i + TRIM) }),
        );
        await expect.poll(() => field("hist")).toBe(history);
      }

      let after = await lineOffset(page, ref!);
      await expect
        .poll(async () => {
          const prev = after;
          after = await lineOffset(page, ref!);
          return after === prev;
        })
        .toBe(true);

      expect(
        after,
        `L${ref} was ${before}px from the top and is no longer rendered: the reader was moved away`,
      ).not.toBeNull();
      expect(Math.abs(after! - before), `L${ref} moved from ${before}px to ${after}px (lineH ${lineH}px)`).toBeLessThan(
        lineH / 2,
      );
    });
  }

  // #4375: a width change re-wraps tmux's rows, so none match the reader's rows one for one.
  test("a re-wrap at another width keeps the reader's line at the top of the view", async ({ page }) => {
    const line = lineKinds[1]!.line;
    const { handle, field, rows, fullWindow } = await scrollIntoHistory(page, line);
    await handle.pushLiveFrame(makeLiveFrame({ rows, history: HISTORY, window: fullWindow, line: vtRow(line) }));
    await expect.poll(() => field("spacer")).toBe(0);
    const before = await topLine(page);
    expect(before, "a history line must be at the top of the view").not.toBeNull();

    // The same lines, re-wrapped by tmux at 100 columns after a resize.
    const history = Array.from({ length: HISTORY }, (_, i) => line(i + 1)).flatMap((l) => l.match(/.{1,100}/g)!);
    const screen = ["$ ready", ...Array.from({ length: rows - 1 }, () => "")];
    await handle.pushLiveFrame({
      content: [...history, ...screen].join("\n") + "\n",
      rows,
      history: history.length,
      cursor: { x: 8, y: 0 },
    });
    await expect.poll(() => field("hist")).toBe(history.length);

    let after = await topLine(page);
    await expect
      .poll(async () => {
        const prev = after;
        after = await topLine(page);
        return after === prev;
      })
      .toBe(true);
    expect(after, `L${before} was at the top of the view before the re-wrap`).toBe(before);
  });
});
