import { test, expect } from "./helpers/mockedTest";
import { devices, type Page } from "@playwright/test";
import { clickSidebarSession, openMobileSidebar } from "./helpers/sidebar";
import {
  mockTerminalApis,
  installTerminalSpies,
  seedSettings,
  fireTouches,
  type MockHandle,
} from "./helpers/terminal-mocks";

// Mobile scrollback on the capture-snapshot live view. Scrolling is the
// browser's NATIVE scroll over rendered history lines (no tmux copy-mode,
// no SGR wheel synthesis, no pause/resume SIGSTOP): the spec asserts the
// live-view contract instead of the old copy-mode one.
test.use({ ...devices["iPhone 13"] });

async function openSession(page: Page, handle: MockHandle) {
  await openMobileSidebar(page);
  await clickSidebarSession(page, "pinch-test");
  await page.locator("[data-live-terminal]").waitFor({ state: "visible", timeout: 10_000 });
  await expect.poll(() => handle.liveMessages.length, { timeout: 5_000 }).toBeGreaterThan(0);
  // Let the first frame land + the sizing effect settle.
  await page.waitForTimeout(400);
}

function scroller(page: Page) {
  return page.locator("[data-live-terminal] > div").first();
}

async function liveLineHeight(page: Page) {
  return scroller(page).evaluate((el) => {
    const rows = el.querySelectorAll("[data-live-content] > div");
    return rows.length >= 2 ? (rows[rows.length - 1] as HTMLElement).getBoundingClientRect().height : 16;
  });
}

// A real, trusted touch flick UP (finger drags DOWN the screen, so content
// scrolls up into scrollback). Playwright's page.touchscreen only taps, and a
// JS-synthesized TouchEvent is untrusted and never natively scrolls; CDP
// Input.dispatchTouchEvent is trusted, so it drives both the React touch
// handlers (touchActiveRef) AND the browser's native scroll, exactly like a
// finger.
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
    await page.goto("/");
    await seedSettings(page, { mobileFontSize: 14 });
    await page.reload();
    await openSession(page, handle);

    // The live-edge capture window covers the screen PLUS a buffer of
    // scrollback (more than one screenful), so history is already rendered
    // ABOVE the live screen instead of a blank spacer. Without the buffer the
    // live-edge window was just the screen and a scroll-up landed on blank
    // until a round-trip filled it.
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

    // Real scrollback text is in the DOM at the live edge (not just the screen).
    await expect.poll(() => page.locator("[data-live-content]").innerText()).toContain("history line");

    // Scroll up one viewport: the revealed rows are real text, already loaded.
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
    await page.goto("/");
    await seedSettings(page, { mobileFontSize: 14 });
    await page.reload();
    await openSession(page, handle);

    // Scroll up into the deep history: the window widens to the full history,
    // hundreds of rows tall.
    await scroller(page).evaluate((el) => {
      el.scrollTop = el.scrollHeight * 0.5;
    });
    await expect.poll(() => scroller(page).evaluate((el) => el.scrollHeight), { timeout: 3_000 }).toBeGreaterThan(8000);

    // Re-center in the deep content and assert only a viewport-ish window of
    // rows is mounted (not all ~600), while scrollHeight still spans the whole
    // history (rows collapse into equal-height padding, not into nothing).
    await scroller(page).evaluate((el) => {
      el.scrollTop = el.scrollHeight * 0.5;
    });
    await page.waitForTimeout(300);
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
    await page.goto("/");
    await seedSettings(page, { mobileFontSize: 14 });
    await page.reload();
    await openSession(page, handle);
    await expect.poll(() => page.locator("[data-live-content]").innerText()).toContain("$ ready");

    await scroller(page).evaluate((el) => {
      el.scrollTop = el.scrollHeight * 0.45;
      el.dispatchEvent(new Event("scroll"));
    });
    await expect.poll(() => scroller(page).evaluate((el) => el.scrollHeight), { timeout: 3_000 }).toBeGreaterThan(8000);

    // Let React render the deep-history viewport. The live tail should still be
    // mounted when the final bottom scroll event flips the pane back to live, so
    // that transition has real rows instead of a spacer-only frame.
    await page.waitForTimeout(300);
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
    await page.goto("/");
    await seedSettings(page, { mobileFontSize: 14 });
    await page.reload();
    await openSession(page, handle);

    await expect(page.getByRole("button", { name: "Back to live" })).toHaveCount(0);

    await scroller(page).evaluate((el) => {
      el.scrollTop = 0;
    });
    const btn = page.getByRole("button", { name: "Back to live" });
    await expect(btn).toBeVisible();

    // History content rendered as real DOM text.
    await expect.poll(() => page.locator("[data-live-content]").innerText()).toContain("history line");

    await btn.tap();
    await expect(btn).toHaveCount(0);
    // Returning to the live edge involves a window-shrink round-trip, so the
    // distance to the bottom converges asynchronously over a few frames; poll
    // it rather than reading once (a single read can land mid-settle).
    await expect
      .poll(() => scroller(page).evaluate((el) => el.scrollHeight - el.scrollTop - el.clientHeight), { timeout: 5_000 })
      .toBeLessThan(30);
  });

  test("scrolling requests a bigger capture window instead of wheel escapes", async ({ page }) => {
    await installTerminalSpies(page);
    const handle = await mockTerminalApis(page);
    await page.goto("/");
    await seedSettings(page, { mobileFontSize: 14 });
    await page.reload();
    await openSession(page, handle);

    const before = textMessages(handle).filter((m) => m.includes('"type":"window"')).length;
    await scroller(page).evaluate((el) => {
      el.scrollTop = 0;
    });
    await expect
      .poll(() => textMessages(handle).filter((m) => m.includes('"type":"window"')).length, { timeout: 3_000 })
      .toBeGreaterThan(before);

    // The copy-mode machinery must stay retired on mobile: no SGR wheel
    // bytes, no pause/resume control messages, ever.
    const all = textMessages(handle).join("");
    expect(all).not.toContain("\x1b[<64;");
    expect(all).not.toContain("\x1b[<65;");
    expect(all).not.toContain("pause_output");
    expect(all).not.toContain("resume_output");
  });

  test("incoming frames never move the scroll position while reading", async ({ page }) => {
    await installTerminalSpies(page);
    const handle = await mockTerminalApis(page);
    await page.goto("/");
    await seedSettings(page, { mobileFontSize: 14 });
    await page.reload();
    await openSession(page, handle);

    // Scroll partway up (a gesture start, not the absolute top), then
    // push frames as if the agent were streaming. Both the gesture-start
    // race (frames pinning under a starting drag) and the browser's
    // native scroll anchoring (re-anchoring when the spacer collapses)
    // historically snapped the viewport; the position must hold.
    const target = await scroller(page).evaluate((el) => {
      el.scrollTop = Math.max(0, el.scrollHeight - el.clientHeight - el.clientHeight * 0.7);
      return el.scrollTop;
    });
    for (let i = 0; i < 4; i++) {
      await page.waitForTimeout(120);
      handle.pushLiveFrame({
        content: Array.from({ length: 24 }, (_, n) => `streamed ${i}-${n}`).join("\n") + "\n",
        rows: 24,
        history: 130 + i,
      });
    }
    await page.waitForTimeout(300);
    const after = await scroller(page).evaluate((el) => el.scrollTop);
    expect(Math.abs(after - target), "scroll position must hold while frames arrive").toBeLessThan(20);
  });

  test("a streamed frame never snaps a reader off the live edge back to the bottom", async ({ page }) => {
    await installTerminalSpies(page);
    const handle = await mockTerminalApis(page);
    await page.goto("/");
    await seedSettings(page, { mobileFontSize: 14 });
    await page.reload();
    await openSession(page, handle);

    // A flick lifts the finger immediately, so the touch-active guard is
    // already gone while iOS momentum carries the scroller up off the
    // live edge. On a busy agent session a live frame lands within ~50ms;
    // pinning there snapped the view back AND killed momentum, which made
    // starting scrollback nearly impossible. Once the reader has left the
    // live edge, a streamed frame must never pin them back to the bottom.
    //
    // Size the gesture in line-heights rather than raw pixels: the bottom
    // threshold is ~1.5 lines, so a fixed pixel nudge lands inside it on a
    // tall font metric and outside on a short one (#2087's original 10/15px
    // nudges were below 1.5 lines at the CI font scale and flaked). The
    // mocked harness also cannot reproduce continuous iOS momentum, so a
    // frame arriving in a quiescent gap between discrete scroll mutations
    // can momentarily look "not moving"; clearing the threshold up front
    // keeps the assertion deterministic.
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
    handle.pushLiveFrame({
      content: Array.from({ length: 24 }, (_, n) => `busy ${n}`).join("\n") + "\n",
      rows: 24,
      history: 130,
    });
    await page.waitForTimeout(150);
    // The flick carries a little further up; another frame arrives.
    await scroller(page).evaluate((el, step) => {
      el.scrollTop -= step;
    }, Math.ceil(lineH));
    handle.pushLiveFrame({
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
    await page.goto("/");
    await seedSettings(page, { mobileFontSize: 14 });
    await page.reload();
    await openSession(page, handle);

    // Trusted touch gesture: a genuine flick up off the live edge, the actual
    // reported scenario. The herky-jerky bug was that once the finger lifted,
    // a streamed frame re-pinned the scroller to the bottom and cancelled the
    // gesture; the reader must stay where the flick left them.
    await touchFlickUp(page, 220);
    await page.waitForTimeout(120);
    const afterFlick = await scroller(page).evaluate((el) => el.scrollTop);
    const lineH = await liveLineHeight(page);
    // The flick actually left the live edge (sanity: native scroll happened).
    const distAfterFlick = await scroller(page).evaluate((el) => el.scrollHeight - el.scrollTop - el.clientHeight);
    expect(distAfterFlick, "the flick scrolled up off the live edge").toBeGreaterThan(lineH * 2);

    // Agent keeps streaming while the user reads.
    for (let i = 0; i < 4; i++) {
      handle.pushLiveFrame({
        content: Array.from({ length: 24 }, (_, n) => `streamed ${i}-${n}`).join("\n") + "\n",
        rows: 24,
        history: 130 + i,
      });
      await page.waitForTimeout(120);
    }

    // The reader holds position: a streamed frame must never pull scrollTop
    // back down toward the live edge.
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
    await page.goto("/");
    await seedSettings(page, { mobileFontSize: 14 });
    await page.reload();
    await openSession(page, handle);

    // The precise regression: a tiny scroll-up that lands INSIDE the ~1.5-line
    // at-bottom tolerance. The old pin treated this as "still live" and, on the
    // next streamed frame where it saw no per-frame upward motion, snapped the
    // scroller back to the bottom. That dead-zone fight is the herky-jerky
    // stutter felt before scrolling could get going. Deterministic (no momentum)
    // so it stays a stable discriminator. The sticky detach latch must hold.
    const lineH = await liveLineHeight(page);
    const placed = await scroller(page).evaluate((el, lh) => {
      el.scrollTop = el.scrollHeight - el.clientHeight - lh; // one line up: inside the dead zone
      el.dispatchEvent(new Event("scroll"));
      return el.scrollTop;
    }, lineH);

    // Stream same-geometry frames (one prompt row + blanks, constant history)
    // so the live target stays put and the 1-line offset stays inside the dead
    // zone. Only the prompt text varies, to force a re-render+pin. With the old
    // pin this snapped scrollTop back to the bottom on the second such frame.
    for (let i = 0; i < 4; i++) {
      handle.pushLiveFrame({ content: `$ ready ${i}\n` + "\n".repeat(23), rows: 24, history: 120 });
      await page.waitForTimeout(120);
    }

    const after = await scroller(page).evaluate((el) => el.scrollTop);
    expect(after, "a one-line scroll-up must not be snapped back to the bottom").toBeLessThanOrEqual(placed + 2);
  });

  test("a streamed frame does not pin away the first pixels of an upward flick", async ({ page }) => {
    await installTerminalSpies(page);
    const handle = await mockTerminalApis(page);
    await page.goto("/");
    await seedSettings(page, { mobileFontSize: 14 });
    await page.reload();
    await openSession(page, handle);

    // Real-device flutter: iOS momentum starts slow, so a streamed frame can
    // land while a flick has only moved a pixel or two, INSIDE the detach
    // latch's ~2px threshold. The old follow-pin treated that as "still live"
    // and snapped scrollTop back to the bottom, which also cancels iOS momentum,
    // so gentle flicks die in their first pixels. Simulate that window: nudge up
    // a pixel at a time, streaming a same-geometry frame after each. The
    // direction guard must let the position accumulate upward instead of being
    // pinned back to the bottom every frame.
    await scroller(page).evaluate((el) => {
      el.scrollTop = el.scrollHeight;
    });
    for (let i = 0; i < 8; i++) {
      await scroller(page).evaluate((el) => {
        el.scrollTop = el.scrollTop - 1;
        el.dispatchEvent(new Event("scroll"));
      });
      handle.pushLiveFrame({ content: `$ ready ${i}\n` + "\n".repeat(23), rows: 24, history: 120 });
      await page.waitForTimeout(40);
    }
    const dist = await scroller(page).evaluate((el) => el.scrollHeight - el.scrollTop - el.clientHeight);
    expect(dist, "the upward nudges accumulate; the pin did not cancel them").toBeGreaterThan(4);
  });

  test("a touch-drag switches to the anchored reading window immediately", async ({ page }) => {
    await installTerminalSpies(page);
    const handle = await mockTerminalApis(page);
    await page.goto("/");
    await seedSettings(page, { mobileFontSize: 14 });
    await page.reload();
    await openSession(page, handle);

    // At the live edge the capture window is "the bottom N lines", so every
    // streamed line slides it and re-renders every row under the finger (the
    // flash). Grabbing the scroller to drag must switch to the reading model
    // right away (anchored window + idle cadence) so the tail stops sliding.
    // A drag is a touchstart followed by real movement; a tap (no movement)
    // must NOT trigger it.
    const windowMsgs = () => textMessages(handle).filter((m) => m.includes('"type":"window"')).length;

    // A tap (touchstart + touchend, no move) does not enter reading.
    await fireTouches(page, "touchstart", [{ x: 30, y: 120 }]);
    await fireTouches(page, "touchend", []);
    await page.waitForTimeout(100);
    const afterTap = windowMsgs();

    // A drag (touchstart + a >8px move) switches to the reading window.
    await fireTouches(page, "touchstart", [{ x: 30, y: 120 }]);
    await fireTouches(page, "touchmove", [{ x: 30, y: 160 }]);
    await expect.poll(windowMsgs, { timeout: 2_000 }).toBeGreaterThan(afterTap);
    await expect(page.getByRole("button", { name: "Back to live" })).toBeVisible();
  });

  test("reading keeps the stream flowing (no hold/freeze)", async ({ page }) => {
    await installTerminalSpies(page);
    const handle = await mockTerminalApis(page);
    await page.goto("/");
    await seedSettings(page, { mobileFontSize: 14 });
    await page.reload();
    await openSession(page, handle);

    // Scrolling up widens the capture window but never freezes the pane:
    // mirroring the TUI's live mode, the agent keeps running and frames
    // keep arriving while the user reads. The client must NOT send a
    // hold (the whole freeze path is gone); it drops cadence to idle.
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

    // A streamed frame still renders while reading (pane is not frozen). Rows
    // are virtualized, so the new content must land WHERE the reader is looking
    // (top of a fully-fetched frame, no spacer) to be in the mounted window
    // rather than off-screen at the live tail.
    handle.pushLiveFrame({
      content: Array.from({ length: 74 }, (_, n) => `still streaming ${n}`).join("\n") + "\n",
      rows: 24,
      history: 50,
    });
    await expect.poll(() => page.locator("[data-live-content]").innerText()).toContain("still streaming");
  });
});
