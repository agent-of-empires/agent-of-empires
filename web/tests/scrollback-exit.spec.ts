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

  test("a deep history is virtualized, and jumping to the bottom shows no blank spacer frame", async ({ page }) => {
    await installTerminalSpies(page);
    const handle = await mockTerminalApis(page, { liveHistory: 600 });
    await openSession(page, handle);
    await expect.poll(() => page.locator("[data-live-content]").innerText()).toContain("$ ready");

    await scroller(page).evaluate((el) => {
      el.scrollTop = el.scrollHeight * 0.45;
      el.dispatchEvent(new Event("scroll"));
    });
    await expect.poll(() => scroller(page).evaluate((el) => el.scrollHeight), { timeout: 3_000 }).toBeGreaterThan(8000);

    // Rows are virtualized: only a window of the deep history is mounted.
    await expect(page.locator("[data-live-content]")).toContainText("history line");
    const mounted = await scroller(page).evaluate((el) => el.querySelectorAll("[data-live-content] > div").length);
    expect(mounted, "only a window of the deep history is mounted").toBeLessThan(250);

    // Keep the live tail mounted so the flip back to live renders rows, not a spacer.
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

  test("a real touch flick into scrollback is not yanked back by streaming frames", async ({ page }) => {
    await installTerminalSpies(page);
    const handle = await mockTerminalApis(page);
    await openSession(page, handle);

    await touchFlickUp(page, 220);
    const lineH = await liveLineHeight(page);
    await expect
      .poll(() => scroller(page).evaluate((el) => el.scrollHeight - el.scrollTop - el.clientHeight), {
        message: "the flick scrolled up off the live edge",
      })
      .toBeGreaterThan(lineH * 2);
    const afterFlick = await scroller(page).evaluate((el) => el.scrollTop);

    for (let i = 0; i < 4; i++) {
      await handle.pushLiveFrame({
        content: Array.from({ length: 24 }, (_, n) => `streamed ${i}-${n}`).join("\n") + "\n",
        rows: 24,
        history: 130 + i,
      });
      await expect(page.locator("[data-live-content]")).toContainText(`streamed ${i}-0`);
      expect(await scroller(page).evaluate((el) => el.scrollTop)).toBeLessThanOrEqual(afterFlick + 2);
    }

    const afterFrames = await scroller(page).evaluate((el) => el.scrollTop);
    expect(afterFrames, "streaming frames must not drag the reader back toward the bottom").toBeLessThanOrEqual(
      afterFlick + 2,
    );
    const distAfterFrames = await scroller(page).evaluate((el) => el.scrollHeight - el.scrollTop - el.clientHeight);
    expect(distAfterFrames, "the reader stays in scrollback").toBeGreaterThan(lineH);
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
      await expect(page.locator("[data-live-content]")).toContainText(`$ ready ${i}`);
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

  test("reading requests a bigger window and keeps the stream flowing (no hold/freeze)", async ({ page }) => {
    await installTerminalSpies(page);
    const handle = await mockTerminalApis(page);
    await openSession(page, handle);

    // Reading never freezes the pane: no hold is sent, only an idle cadence.
    const before = textMessages(handle).filter((m) => m.includes('"type":"window"')).length;
    await scroller(page).evaluate((el) => {
      el.scrollTop = 0;
    });
    await expect
      .poll(() => textMessages(handle).filter((m) => m.includes('"type":"window"')).length, { timeout: 3_000 })
      .toBeGreaterThan(before);
    await expect
      .poll(() => {
        const msgs = textMessages(handle).filter((m) => m.includes('"type":"cadence"'));
        return msgs[msgs.length - 1] ?? "";
      })
      .toContain('"fast":false');

    // Reading is a bigger capture window, never wheel escapes or pause/hold control messages.
    const all = textMessages(handle).join("");
    expect(all, "the hold control message is retired").not.toContain('"type":"hold"');
    for (const escape of ["\x1b[<64;", "\x1b[<65;", "pause_output", "resume_output"]) expect(all).not.toContain(escape);

    // Rows are virtualized, so the new frame must render where the reader is looking.
    await handle.pushLiveFrame({
      content: Array.from({ length: 74 }, (_, n) => `still streaming ${n}`).join("\n") + "\n",
      rows: 24,
      history: 50,
    });
    await expect.poll(() => page.locator("[data-live-content]").innerText()).toContain("still streaming");
  });
});
