import { test, expect } from "./helpers/mockedTest";
import { openLiveSession, scroller } from "./helpers/liveTerminal";
import { devices, type Page } from "@playwright/test";
import { mockTerminalApis, installTerminalSpies, fireTouches, type MockHandle } from "./helpers/terminal-mocks";

// Mobile scrollback on the live view is native scrolling over rendered history, never tmux copy-mode.
test.use({ ...devices["iPhone 13"] });

const openSession = (page: Page, handle: MockHandle) =>
  openLiveSession(page, handle, { mobile: true, settings: { mobileFontSize: 14 } });

async function liveLineHeight(page: Page) {
  return scroller(page).evaluate((el) => {
    const rows = el.querySelectorAll("[data-live-content] > div");
    return rows.length >= 2 ? (rows[rows.length - 1] as HTMLElement).getBoundingClientRect().height : 16;
  });
}

// A trusted CDP touch flick up; synthesized TouchEvents never scroll natively.
async function touchFlickUp(page: Page, distance: number, steps = 8) {
  const client = await page.context().newCDPSession(page);
  const box = await scroller(page).boundingBox();
  if (!box) throw new Error("no scroller box");
  const x = box.x + box.width / 2;
  let y = box.y + box.height * 0.25;
  await client.send("Input.dispatchTouchEvent", { type: "touchStart", touchPoints: [{ x, y }] });
  for (let i = 0; i < steps; i++) {
    y += distance / steps;
    await client.send("Input.dispatchTouchEvent", { type: "touchMove", touchPoints: [{ x, y }] });
    await page.waitForTimeout(16);
  }
  await client.send("Input.dispatchTouchEvent", { type: "touchEnd", touchPoints: [] });
}

function textMessages(handle: MockHandle): string[] {
  return handle.liveMessages.map((m) => m.toString("utf8"));
}

test.describe("Mobile live-view scrollback", () => {
  test("keeps recent scrollback loaded at the live edge so a scroll-up is not blank", async ({ page }) => {
    await installTerminalSpies(page);
    const handle = await mockTerminalApis(page);
    await openSession(page, handle);

    // The live-edge window includes more than a screenful of history, so scrolling up lands on text.
    const screenRows = await scroller(page).evaluate((el) => {
      const rows = el.querySelectorAll("[data-live-content] > div");
      const h = rows.length >= 2 ? (rows[rows.length - 1] as HTMLElement).getBoundingClientRect().height : 16;
      return Math.round(el.clientHeight / h);
    });
    const lastWindow = Number(
      (
        textMessages(handle)
          .filter((m) => m.includes('"type":"window"'))
          .pop() ?? "{}"
      ).match(/"lines":(\d+)/)?.[1] ?? "0",
    );
    expect(lastWindow, "live-edge window covers more than one screen").toBeGreaterThan(screenRows);

    await expect.poll(() => page.locator("[data-live-content]").innerText()).toContain("history line");

    await scroller(page).evaluate((el) => {
      el.scrollTop = Math.max(0, el.scrollHeight - 2 * el.clientHeight);
    });
    const visibleText = await scroller(page).evaluate((el) => {
      const rows = Array.from(el.querySelectorAll("[data-live-content] > div")) as HTMLElement[];
      const top = el.scrollTop;
      const bottom = top + el.clientHeight;
      return rows
        .filter((r) => r.offsetTop >= top && r.offsetTop < bottom)
        .map((r) => r.textContent ?? "")
        .join("|");
    });
    expect(visibleText, "a scroll-up shows loaded scrollback, not blank").toContain("history line");
  });

  test("reading a deep history mounts only a window of rows (virtualized)", async ({ page }) => {
    await installTerminalSpies(page);
    const handle = await mockTerminalApis(page, { liveHistory: 600 });
    await openSession(page, handle);

    await scroller(page).evaluate((el) => {
      el.scrollTop = el.scrollHeight * 0.5;
    });
    await expect.poll(() => scroller(page).evaluate((el) => el.scrollHeight), { timeout: 3_000 }).toBeGreaterThan(8000);

    // Rows are virtualized: only a window is mounted while scrollHeight spans the history.
    await scroller(page).evaluate((el) => {
      el.scrollTop = el.scrollHeight * 0.5;
    });
    await expect
      .poll(() =>
        scroller(page).evaluate((el) => {
          const pane = el.getBoundingClientRect();
          return Array.from(el.querySelectorAll("[data-live-content] > div")).some((row) => {
            const rect = row.getBoundingClientRect();
            return rect.bottom > pane.top && rect.top < pane.bottom && row.textContent?.includes("history line");
          });
        }),
      )
      .toBe(true);
    const m = await scroller(page).evaluate((el) => ({
      mounted: el.querySelectorAll("[data-live-content] > div").length,
      scrollHeight: el.scrollHeight,
    }));
    expect(m.mounted, "only a window of the deep history is mounted").toBeLessThan(250);
    expect(m.scrollHeight, "the document still spans the full history").toBeGreaterThan(8000);
  });

  test("jumping to the bottom while reading does not show a blank spacer frame", async ({ page }) => {
    await installTerminalSpies(page);
    const handle = await mockTerminalApis(page, { liveHistory: 600 });
    await openSession(page, handle);
    await expect.poll(() => page.locator("[data-live-content]").innerText()).toContain("$ ready");

    await scroller(page).evaluate((el) => {
      el.scrollTop = el.scrollHeight * 0.45;
      el.dispatchEvent(new Event("scroll"));
    });
    await expect.poll(() => scroller(page).evaluate((el) => el.scrollHeight), { timeout: 3_000 }).toBeGreaterThan(8000);

    // Keep the live tail mounted so the flip back to live renders rows, not a spacer.
    await expect(page.locator("[data-live-content]")).toContainText("history line");
    const visibleText = await scroller(page).evaluate((el) => {
      el.scrollTop = el.scrollHeight - el.clientHeight;
      el.dispatchEvent(new Event("scroll"));
      const rows = Array.from(el.querySelectorAll("[data-live-content] > div")) as HTMLElement[];
      const top = el.scrollTop;
      const bottom = top + el.clientHeight;
      return rows
        .filter((r) => r.offsetTop >= top && r.offsetTop < bottom)
        .map((r) => r.textContent ?? "")
        .join("|");
    });
    expect(visibleText, "bottom jump shows live-tail rows, not only spacer padding").toContain("$ ready");
  });

  test("scrolling up shows Back to live; tapping it returns to the bottom", async ({ page }) => {
    await installTerminalSpies(page);
    const handle = await mockTerminalApis(page);
    await openSession(page, handle);

    await expect(page.getByRole("button", { name: "Back to live" })).toHaveCount(0);

    await scroller(page).evaluate((el) => {
      el.scrollTop = 0;
    });
    const btn = page.getByRole("button", { name: "Back to live" });
    await expect(btn).toBeVisible();

    await expect.poll(() => page.locator("[data-live-content]").innerText()).toContain("history line");

    await btn.tap();
    await expect(btn).toHaveCount(0);
    // The distance to the bottom settles over a few frames.
    await expect
      .poll(() => scroller(page).evaluate((el) => el.scrollHeight - el.scrollTop - el.clientHeight), { timeout: 5_000 })
      .toBeLessThan(30);
  });

  test("scrolling requests a bigger capture window instead of wheel escapes", async ({ page }) => {
    await installTerminalSpies(page);
    const handle = await mockTerminalApis(page);
    await openSession(page, handle);

    const before = textMessages(handle).filter((m) => m.includes('"type":"window"')).length;
    await scroller(page).evaluate((el) => {
      el.scrollTop = 0;
    });
    await expect
      .poll(() => textMessages(handle).filter((m) => m.includes('"type":"window"')).length, { timeout: 3_000 })
      .toBeGreaterThan(before);

    // No SGR wheel bytes or pause/resume control messages on mobile.
    const all = textMessages(handle).join("");
    expect(all).not.toContain("\x1b[<64;");
    expect(all).not.toContain("\x1b[<65;");
    expect(all).not.toContain("pause_output");
    expect(all).not.toContain("resume_output");
  });

  test("incoming frames never move the scroll position while reading", async ({ page }) => {
    await installTerminalSpies(page);
    const handle = await mockTerminalApis(page);
    await openSession(page, handle);

    // Streaming frames must not snap a scroll that has started, via pinning or scroll anchoring.
    const target = await scroller(page).evaluate((el) => {
      el.scrollTop = Math.max(0, el.scrollHeight - el.clientHeight - el.clientHeight * 0.7);
      return el.scrollTop;
    });
    for (let i = 0; i < 4; i++) {
      await page.waitForTimeout(120);
      await handle.pushLiveFrame({
        content: Array.from({ length: 24 }, (_, n) => `streamed ${i}-${n}`).join("\n") + "\n",
        rows: 24,
        history: 130 + i,
      });
      expect(Math.abs((await scroller(page).evaluate((el) => el.scrollTop)) - target)).toBeLessThan(20);
    }
    await page.waitForTimeout(300);
    const after = await scroller(page).evaluate((el) => el.scrollTop);
    expect(Math.abs(after - target), "scroll position must hold while frames arrive").toBeLessThan(20);
  });

  test("a streamed frame never snaps a reader off the live edge back to the bottom", async ({ page }) => {
    await installTerminalSpies(page);
    const handle = await mockTerminalApis(page);
    await openSession(page, handle);

    // Momentum continues after the finger lifts, so off the live edge a streamed frame must never pin to the bottom.
    // The nudge is sized in lines because the bottom threshold is about 1.5 lines (#2087).
    const lineH = await scroller(page).evaluate((el) => {
      const rows = el.querySelectorAll("[data-live-content] > div");
      return rows.length >= 2 ? (rows[rows.length - 1] as HTMLElement).getBoundingClientRect().height : 16;
    });
    const start = await scroller(page).evaluate(
      (el, up) => {
        el.scrollTop = el.scrollHeight - el.clientHeight - up;
        return el.scrollTop;
      },
      Math.ceil(lineH * 3),
    );
    await handle.pushLiveFrame({
      content: Array.from({ length: 24 }, (_, n) => `busy ${n}`).join("\n") + "\n",
      rows: 24,
      history: 130,
    });
    await page.waitForTimeout(150);
    await scroller(page).evaluate((el, step) => {
      el.scrollTop -= step;
    }, Math.ceil(lineH));
    await handle.pushLiveFrame({
      content: Array.from({ length: 24 }, (_, n) => `busy2 ${n}`).join("\n") + "\n",
      rows: 24,
      history: 131,
    });
    await page.waitForTimeout(200);
    const distance = await scroller(page).evaluate((el) => el.scrollHeight - el.scrollTop - el.clientHeight);
    expect(distance, "the reader stays in scrollback, not snapped to the live edge").toBeGreaterThan(lineH);
    const after = await scroller(page).evaluate((el) => el.scrollTop);
    expect(after, "a streamed frame must not pin the reader back below the gesture").toBeLessThan(start);
  });

  test("a real touch flick into scrollback is not yanked back by streaming frames", async ({ page }) => {
    await installTerminalSpies(page);
    const handle = await mockTerminalApis(page);
    await openSession(page, handle);

    await touchFlickUp(page, 220);
    await page.waitForTimeout(120);
    const afterFlick = await scroller(page).evaluate((el) => el.scrollTop);
    const lineH = await liveLineHeight(page);
    const distAfterFlick = await scroller(page).evaluate((el) => el.scrollHeight - el.scrollTop - el.clientHeight);
    expect(distAfterFlick, "the flick scrolled up off the live edge").toBeGreaterThan(lineH * 2);

    for (let i = 0; i < 4; i++) {
      await handle.pushLiveFrame({
        content: Array.from({ length: 24 }, (_, n) => `streamed ${i}-${n}`).join("\n") + "\n",
        rows: 24,
        history: 130 + i,
      });
      expect(await scroller(page).evaluate((el) => el.scrollTop)).toBeLessThanOrEqual(afterFlick + 2);
      await page.waitForTimeout(120);
    }

    const afterFrames = await scroller(page).evaluate((el) => el.scrollTop);
    expect(afterFrames, "streaming frames must not drag the reader back toward the bottom").toBeLessThanOrEqual(
      afterFlick + 2,
    );
    const distAfterFrames = await scroller(page).evaluate((el) => el.scrollHeight - el.scrollTop - el.clientHeight);
    expect(distAfterFrames, "the reader stays in scrollback").toBeGreaterThan(lineH);
  });

  test("a frame does not snap a one-line scroll-up back to the live edge", async ({ page }) => {
    await installTerminalSpies(page);
    const handle = await mockTerminalApis(page);
    await openSession(page, handle);

    // A scroll-up inside the 1.5-line at-bottom tolerance must stay detached (the dead-zone stutter).
    const lineH = await liveLineHeight(page);
    const placed = await scroller(page).evaluate((el, lh) => {
      el.scrollTop = el.scrollHeight - el.clientHeight - lh; // one line up: inside the dead zone
      el.dispatchEvent(new Event("scroll"));
      return el.scrollTop;
    }, lineH);

    // Same-geometry frames keep the live target fixed; only the prompt text changes to force a re-render.
    for (let i = 0; i < 4; i++) {
      await handle.pushLiveFrame({ content: `$ ready ${i}\n` + "\n".repeat(23), rows: 24, history: 120 });
      expect(await scroller(page).evaluate((el) => el.scrollTop)).toBeLessThanOrEqual(placed + 2);
      await page.waitForTimeout(120);
    }

    const after = await scroller(page).evaluate((el) => el.scrollTop);
    expect(after, "a one-line scroll-up must not be snapped back to the bottom").toBeLessThanOrEqual(placed + 2);
  });

  test("a streamed frame does not pin away the first pixels of an upward flick", async ({ page }) => {
    await installTerminalSpies(page);
    const handle = await mockTerminalApis(page);
    await openSession(page, handle);

    // iOS momentum starts slowly: pixel-by-pixel upward movement inside the 2px latch must accumulate, not pin.
    await scroller(page).evaluate((el) => {
      el.scrollTop = el.scrollHeight;
    });
    for (let i = 0; i < 8; i++) {
      await scroller(page).evaluate((el) => {
        el.scrollTop = el.scrollTop - 1;
        el.dispatchEvent(new Event("scroll"));
      });
      await handle.pushLiveFrame({ content: `$ ready ${i}\n` + "\n".repeat(23), rows: 24, history: 120 });
      await page.waitForTimeout(40);
    }
    const dist = await scroller(page).evaluate((el) => el.scrollHeight - el.scrollTop - el.clientHeight);
    expect(dist, "the upward nudges accumulate; the pin did not cancel them").toBeGreaterThan(4);
  });

  test("a touch-drag switches to the anchored reading window immediately", async ({ page }) => {
    await installTerminalSpies(page);
    const handle = await mockTerminalApis(page);
    await openSession(page, handle);

    // Dragging (not tapping) switches to the anchored reading window so rows stop sliding under the finger.
    const windowMsgs = () => textMessages(handle).filter((m) => m.includes('"type":"window"')).length;

    await fireTouches(page, "touchstart", [{ x: 30, y: 120 }]);
    await fireTouches(page, "touchend", []);
    await page.waitForTimeout(100);
    const afterTap = windowMsgs();

    await fireTouches(page, "touchstart", [{ x: 30, y: 120 }]);
    await fireTouches(page, "touchmove", [{ x: 30, y: 160 }]);
    await expect.poll(windowMsgs, { timeout: 2_000 }).toBeGreaterThan(afterTap);
    await expect(page.getByRole("button", { name: "Back to live" })).toBeVisible();
  });

  test("reading keeps the stream flowing (no hold/freeze)", async ({ page }) => {
    await installTerminalSpies(page);
    const handle = await mockTerminalApis(page);
    await openSession(page, handle);

    // Reading never freezes the pane: no hold is sent, only an idle cadence.
    await scroller(page).evaluate((el) => {
      el.scrollTop = 0;
    });
    await expect
      .poll(() => textMessages(handle).filter((m) => m.includes('"type":"window"')).length, { timeout: 3_000 })
      .toBeGreaterThan(0);
    await expect
      .poll(() => {
        const msgs = textMessages(handle).filter((m) => m.includes('"type":"cadence"'));
        return msgs[msgs.length - 1] ?? "";
      })
      .toContain('"fast":false');

    const all = textMessages(handle).join("");
    expect(all, "the hold control message is retired").not.toContain('"type":"hold"');

    // Rows are virtualized, so the new frame must render where the reader is looking.
    await handle.pushLiveFrame({
      content: Array.from({ length: 74 }, (_, n) => `still streaming ${n}`).join("\n") + "\n",
      rows: 24,
      history: 50,
    });
    await expect.poll(() => page.locator("[data-live-content]").innerText()).toContain("still streaming");
  });
});
