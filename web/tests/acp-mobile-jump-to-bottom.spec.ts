import { test, expect } from "./helpers/mockedTest";
import { devices, type Locator } from "@playwright/test";
import {
  agentMessageChunk,
  mockAcpSession,
  openStructuredSession,
  stopped,
  waitForComposerConnected,
} from "./helpers/acpMock";

// Only a real wheel or touch gesture drops stick-to-bottom, so dispatch one before scrolling.
async function userScroll(viewport: Locator, top: number) {
  await viewport.evaluate((el, to) => {
    el.dispatchEvent(new WheelEvent("wheel", { deltaY: to < el.scrollTop ? -300 : 300, bubbles: true }));
    el.scrollTop = to;
  }, top);
}

// On a coarse pointer a jump-to-latest button appears while scrolled up.
test.use({ ...devices["iPhone 13"] });

test.describe("mobile jump-to-bottom", () => {
  test("appears while scrolled up in a long transcript and re-pins on tap", async ({ page }) => {
    const longText = Array.from({ length: 120 }, (_, i) => `transcript line ${i}`).join("\n");
    const mock = await mockAcpSession(page, {
      title: "story-jump-bottom",
      initialEvents: [agentMessageChunk(longText), stopped()],
    });
    await openStructuredSession(page, mock);
    await waitForComposerConnected(page);

    const viewport = page.getByTestId("acp-viewport");
    const button = page.getByTestId("acp-jump-to-bottom");

    await expect(button).toBeHidden();

    await userScroll(viewport, 0);
    await expect(button).toBeVisible();

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

    for (let i = 0; i < 6; i++) {
      mock.pushEvents([agentMessageChunk("\n" + Array.from({ length: 12 }, (_, j) => `stream ${i}-${j}`).join("\n"))]);
      await expect(viewport).toContainText(`stream ${i}-11`);
      await expect.poll(isPinned).toBe(true);
    }
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

    // A growing composer shrinks the transcript but keeps it pinned; typing is not a scroll gesture.
    const textarea = page.getByRole("textbox").first();
    const beforeHeight = await textarea.evaluate((el) => el.getBoundingClientRect().height);
    await textarea.fill(Array.from({ length: 8 }, (_, i) => `draft line ${i}`).join("\n"));
    await expect.poll(() => textarea.evaluate((el) => el.getBoundingClientRect().height)).toBeGreaterThan(beforeHeight);
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

    // Idle past the 1.2s timestamp window so only the at-bottom ref applies.
    await page.waitForTimeout(1400);

    // Inject the interim scroll iOS fires during the resize; the pin must undo it.
    await page.getByTestId("composer-collapse-toggle").click();
    await viewport.evaluate((el) => {
      el.scrollTop = 0;
    });
    await expect.poll(isPinned).toBe(true);

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
    await expect.poll(() => viewport.evaluate((el) => el.scrollHeight > el.clientHeight + 40)).toBe(true);
    await userScroll(viewport, 0);
    await expect(page.getByTestId("acp-jump-to-bottom")).toBeVisible();

    await page.reload();
    await waitForComposerConnected(page);
    await expect(page.getByTestId("acp-jump-to-bottom")).toBeVisible();
  });

  test("reopening a session with loadable earlier history still lands at the bottom", async ({ page }) => {
    // The mount-time auto-load of earlier history stamps a scroll anchor; a saved stick intent must still win.
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

    mock.pushEvents([agentMessageChunk("\nlater\nlater\nlater\nlater")]);
    await expect(viewport).toContainText("later");
    const after = await viewport.evaluate((el) => el.scrollTop);
    expect(Math.abs(after - before)).toBeLessThan(4);
    await expect(page.getByTestId("acp-jump-to-bottom")).toBeVisible();
  });
});
