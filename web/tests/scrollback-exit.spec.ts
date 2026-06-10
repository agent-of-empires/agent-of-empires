import { test, expect } from "./helpers/mockedTest";
import { devices, type Page } from "@playwright/test";
import { clickSidebarSession, openMobileSidebar } from "./helpers/sidebar";
import { mockTerminalApis, installTerminalSpies, seedSettings, type MockHandle } from "./helpers/terminal-mocks";

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

function textMessages(handle: MockHandle): string[] {
  return handle.liveMessages.map((m) => m.toString("utf8"));
}

test.describe("Mobile live-view scrollback", () => {
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
    const distance = await scroller(page).evaluate((el) => el.scrollHeight - el.scrollTop - el.clientHeight);
    expect(distance).toBeLessThan(30);
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

  test("a frame landing in the first instants of a scroll-up never snaps back to the bottom", async ({ page }) => {
    await installTerminalSpies(page);
    const handle = await mockTerminalApis(page);
    await page.goto("/");
    await seedSettings(page, { mobileFontSize: 14 });
    await page.reload();
    await openSession(page, handle);

    // A flick lifts the finger immediately, so the touch-active guard is
    // already gone while the scroller is still within the at-bottom
    // threshold. On a busy agent session a live frame lands within ~50ms;
    // pinning there snapped the view back AND killed iOS momentum, which
    // made starting scrollback nearly impossible. Upward motion since the
    // last frame must suppress the pin even inside the threshold.
    const nudged = await scroller(page).evaluate((el) => {
      el.scrollTop = el.scrollHeight - el.clientHeight - 10; // inside the 1.5-line bottom threshold
      return el.scrollTop;
    });
    handle.pushLiveFrame({
      content: Array.from({ length: 24 }, (_, n) => `busy ${n}`).join("\n") + "\n",
      rows: 24,
      history: 130,
    });
    await page.waitForTimeout(200);
    // The momentum continues a little further up; another frame arrives.
    await scroller(page).evaluate((el) => {
      el.scrollTop -= 15;
    });
    handle.pushLiveFrame({
      content: Array.from({ length: 24 }, (_, n) => `busy2 ${n}`).join("\n") + "\n",
      rows: 24,
      history: 131,
    });
    await page.waitForTimeout(200);
    const distance = await scroller(page).evaluate((el) => el.scrollHeight - el.scrollTop - el.clientHeight);
    expect(distance, "the starting gesture must keep its upward progress").toBeGreaterThanOrEqual(20);
    const after = await scroller(page).evaluate((el) => el.scrollTop);
    expect(after, "the scroller must not be pinned back to the bottom").toBeLessThanOrEqual(nudged);
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

    // A streamed frame still renders while reading (pane is not frozen).
    handle.pushLiveFrame({
      content: Array.from({ length: 24 }, (_, n) => `still streaming ${n}`).join("\n") + "\n",
      rows: 24,
      history: 200,
    });
    await expect.poll(() => page.locator("[data-live-content]").innerText()).toContain("still streaming");
  });
});
