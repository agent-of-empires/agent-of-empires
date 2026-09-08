import { test, expect } from "./helpers/mockedTest";
import { devices, type Locator } from "@playwright/test";
import {
  agentMessageChunk,
  mockAcpSession,
  openStructuredSession,
  stopped,
  waitForComposerConnected,
} from "./helpers/acpMock";

// Scroll the way a user does: the structured view only drops "stick to bottom"
// on a real scroll gesture (wheel/touch), not a bare programmatic scrollTo, so
// a test that just set scrollTop would be (correctly) ignored on a coarse
// pointer. Dispatch a wheel event to register the gesture, then move.
async function userScroll(viewport: Locator, top: number) {
  await viewport.evaluate((el, to) => {
    el.dispatchEvent(new WheelEvent("wheel", { deltaY: to < el.scrollTop ? -300 : 300, bubbles: true }));
    el.scrollTop = to;
  }, top);
}

// User story: on a phone, scrolling up to read earlier transcript strands the
// user away from the live bottom. A quick "jump to latest" button appears while
// scrolled up and re-pins to the bottom on tap. Mocked (not live) because this
// is pure client-side scroll layout; iPhone 13 emulation gives the coarse
// pointer the button is gated on.
test.use({ ...devices["iPhone 13"] });

test.describe("mobile jump-to-bottom", () => {
  test("appears while scrolled up in a long transcript and re-pins on tap", async ({ page }) => {
    // A transcript tall enough to overflow the phone viewport, so there is
    // somewhere to scroll up to.
    const longText = Array.from({ length: 120 }, (_, i) => `transcript line ${i}`).join("\n");
    const mock = await mockAcpSession(page, {
      title: "story-jump-bottom",
      initialEvents: [agentMessageChunk(longText), stopped()],
    });
    await openStructuredSession(page, mock);
    await waitForComposerConnected(page);

    const viewport = page.getByTestId("acp-viewport");
    const button = page.getByTestId("acp-jump-to-bottom");

    // On load, scroll restoration pins to the bottom, so the button is not shown.
    await expect(button).toBeHidden();

    // Scroll to the top (real gesture): the button appears.
    await userScroll(viewport, 0);
    await expect(button).toBeVisible();

    // Tapping it returns to the bottom and hides the button.
    await button.click();
    await expect(button).toBeHidden();
    await expect
      .poll(() => viewport.evaluate((el) => el.scrollTop + el.clientHeight >= el.scrollHeight - 16))
      .toBe(true);
  });

  test("sticks to the bottom as new content streams in", async ({ page }) => {
    const mock = await mockAcpSession(page, {
      title: "story-stick",
      initialEvents: [agentMessageChunk("start"), stopped()],
    });
    await openStructuredSession(page, mock);
    await waitForComposerConnected(page);

    const viewport = page.getByTestId("acp-viewport");
    const isPinned = () => viewport.evaluate((el) => el.scrollTop + el.clientHeight >= el.scrollHeight - 16);

    // Stream several tall chunks while the reader is at the bottom.
    for (let i = 0; i < 6; i++) {
      mock.pushEvents([agentMessageChunk("\n" + Array.from({ length: 12 }, (_, j) => `stream ${i}-${j}`).join("\n"))]);
      await expect.poll(isPinned).toBe(true);
    }
    // Never had to reach for the button: it stays hidden the whole time.
    await expect(page.getByTestId("acp-jump-to-bottom")).toBeHidden();
  });

  test("keeps the transcript pinned as the composer grows while typing", async ({ page }) => {
    const longText = Array.from({ length: 120 }, (_, i) => `line ${i}`).join("\n");
    const mock = await mockAcpSession(page, {
      title: "story-grow",
      initialEvents: [agentMessageChunk(longText), stopped()],
    });
    await openStructuredSession(page, mock);
    await waitForComposerConnected(page);

    const viewport = page.getByTestId("acp-viewport");
    const isPinned = () => viewport.evaluate((el) => el.scrollTop + el.clientHeight >= el.scrollHeight - 16);
    await expect.poll(isPinned).toBe(true);

    // A tall multi-line draft grows the composer; the transcript must stay
    // pinned to the bottom (it shrinks to fit above the input) rather than being
    // covered by the growing box. Typing is not a scroll gesture, so it never
    // drops the stick intent.
    const textarea = page.getByRole("textbox").first();
    await textarea.fill(Array.from({ length: 8 }, (_, i) => `draft line ${i}`).join("\n"));
    await expect.poll(isPinned).toBe(true);
  });

  test("re-pins to the bottom across a composer (hide-input) collapse toggle", async ({ page }) => {
    const longText = Array.from({ length: 120 }, (_, i) => `line ${i}`).join("\n");
    const mock = await mockAcpSession(page, {
      title: "story-collapse-pin",
      initialEvents: [agentMessageChunk(longText), stopped()],
    });
    await openStructuredSession(page, mock);
    await waitForComposerConnected(page);

    const viewport = page.getByTestId("acp-viewport");
    const isPinned = () => viewport.evaluate((el) => el.scrollTop + el.clientHeight >= el.scrollHeight - 16);
    await expect.poll(isPinned).toBe(true);

    // Sit idle at the bottom past the fallback timestamp window (1.2s) with no
    // scrolling, so only the "were we at the bottom" ref keeps us covered. This
    // is the sit-then-toggle case that the old timestamp-only guard missed and
    // that made the snap intermittent.
    await page.waitForTimeout(1400);

    // Toggle the "hide text input" bar, then inject an interim scroll-up like the
    // one iOS fires during the resize. The pin-across-transition effect must drag
    // it back to the bottom; without it, the injected scroll would stick.
    await page.getByTestId("composer-collapse-toggle").click();
    await viewport.evaluate((el) => {
      el.scrollTop = 0;
    });
    await expect.poll(isPinned).toBe(true);

    // Show it again, same interim-scroll intrusion, same expectation.
    await page.getByTestId("composer-collapse-toggle").click();
    await viewport.evaluate((el) => {
      el.scrollTop = 0;
    });
    await expect.poll(isPinned).toBe(true);
  });

  test("reopening (reload) shows the bottom by default", async ({ page }) => {
    const longText = Array.from({ length: 120 }, (_, i) => `line ${i}`).join("\n");
    const mock = await mockAcpSession(page, {
      title: "story-reopen-bottom",
      initialEvents: [agentMessageChunk(longText), stopped()],
    });
    await openStructuredSession(page, mock);
    await waitForComposerConnected(page);

    const viewport = page.getByTestId("acp-viewport");
    const isPinned = () => viewport.evaluate((el) => el.scrollTop + el.clientHeight >= el.scrollHeight - 16);
    await expect.poll(isPinned).toBe(true);

    // Reopen the PWA (hard reload of the deep-linked session, localStorage kept).
    await page.reload();
    await waitForComposerConnected(page);
    await expect.poll(isPinned).toBe(true);
    await expect(page.getByTestId("acp-jump-to-bottom")).toBeHidden();
  });

  test("reopening (reload) preserves a scrolled-up position", async ({ page }) => {
    const longText = Array.from({ length: 200 }, (_, i) => `transcript history line number ${i}`).join("\n");
    const mock = await mockAcpSession(page, {
      title: "story-reopen-up",
      initialEvents: [agentMessageChunk(longText), stopped()],
    });
    await openStructuredSession(page, mock);
    await waitForComposerConnected(page);

    const viewport = page.getByTestId("acp-viewport");
    // Wait until the transcript actually overflows, so scrolling up is meaningful.
    await expect.poll(() => viewport.evaluate((el) => el.scrollHeight > el.clientHeight + 40)).toBe(true);
    // Scroll up with a real gesture so the stick intent releases (and is saved).
    await userScroll(viewport, 0);
    await expect(page.getByTestId("acp-jump-to-bottom")).toBeVisible();

    // Reopen: because we were scrolled up when hidden, it must NOT jump to the
    // bottom; the jump-to-bottom button is still shown.
    await page.reload();
    await waitForComposerConnected(page);
    await expect(page.getByTestId("acp-jump-to-bottom")).toBeVisible();
  });

  test("reopening a session with loadable earlier history still lands at the bottom", async ({ page }) => {
    // 100 turns (200 activity rows) overflow the 150-row window, so
    // canLoadEarlierHistory is true. On reopen the initial sample() runs at
    // scrollTop 0 (before the restore pins) and trips the auto-load-earlier,
    // which stamps a scroll anchor. Stick-to-bottom intent must win over that
    // anchor so the session opens pinned to the latest message instead of
    // stranded scrolled up. Regression for the mount-time anchor race that made
    // reopen land scrolled up despite a bottom-stick intent.
    const userPrompt = (text: string) => ({ UserPromptSent: { text } });
    const events: unknown[] = [];
    for (let i = 0; i < 100; i++) {
      events.push(userPrompt(`prompt number ${i}`), agentMessageChunk(`reply number ${i}`), stopped());
    }
    const mock = await mockAcpSession(page, { title: "story-reopen-history", initialEvents: events });
    await openStructuredSession(page, mock);
    await waitForComposerConnected(page);

    const viewport = page.getByTestId("acp-viewport");
    const isPinned = () => viewport.evaluate((el) => el.scrollTop + el.clientHeight >= el.scrollHeight - 16);
    await expect.poll(isPinned).toBe(true);

    // Reopen (hard reload, localStorage kept). Must still land at the bottom
    // even though loadable earlier history triggers the mount auto-load.
    await page.reload();
    await waitForComposerConnected(page);
    await expect.poll(isPinned).toBe(true);
    await expect(page.getByTestId("acp-jump-to-bottom")).toBeHidden();
  });

  test("does not yank a scrolled-up reader to the bottom on new content", async ({ page }) => {
    const longText = Array.from({ length: 120 }, (_, i) => `history line ${i}`).join("\n");
    const mock = await mockAcpSession(page, {
      title: "story-noyank",
      initialEvents: [agentMessageChunk(longText), stopped()],
    });
    await openStructuredSession(page, mock);
    await waitForComposerConnected(page);

    const viewport = page.getByTestId("acp-viewport");
    await userScroll(viewport, 0);
    await expect(page.getByTestId("acp-jump-to-bottom")).toBeVisible();
    const before = await viewport.evaluate((el) => el.scrollTop);

    // New content arriving at the bottom must not move a reader who scrolled up.
    mock.pushEvents([agentMessageChunk("\nlater\nlater\nlater\nlater")]);
    await page.waitForTimeout(150);
    const after = await viewport.evaluate((el) => el.scrollTop);
    expect(Math.abs(after - before)).toBeLessThan(4);
    await expect(page.getByTestId("acp-jump-to-bottom")).toBeVisible();
  });
});
