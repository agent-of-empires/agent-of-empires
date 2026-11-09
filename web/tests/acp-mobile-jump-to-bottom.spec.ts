import { test, expect, observeFor } from "./helpers/mockedTest";
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
  test("appears while scrolled up in a long transcript, survives a reload, and re-pins on tap", async ({ page }) => {
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

    const readingTop = 240;
    await userScroll(viewport, readingTop);
    await expect(button).toBeVisible();
    await expect.poll(() => viewport.evaluate((el) => el.scrollTop)).toBe(readingTop);

    // Reopening (reload) preserves the scrolled-up position.
    await page.reload();
    await waitForComposerConnected(page);
    await expect(button).toBeVisible();
    await expect.poll(() => viewport.evaluate((el) => el.scrollTop)).toBe(readingTop);

    await button.click();
    await expect(button).toBeHidden();
    await expect
      .poll(() => viewport.evaluate((el) => el.scrollTop + el.clientHeight >= el.scrollHeight - 16))
      .toBe(true);
  });

  test("follows output arriving on the jump's first scroll and still permits reader escape", async ({ page }) => {
    const mock = await mockAcpSession(page, {
      title: "story-jump-during-stream",
      initialEvents: [
        agentMessageChunk(Array.from({ length: 120 }, (_, i) => `History ${i}.`).join("\n\n")),
        stopped(),
      ],
    });
    await openStructuredSession(page, mock);
    await waitForComposerConnected(page);

    const viewport = page.getByTestId("acp-viewport");
    const button = page.getByTestId("acp-jump-to-bottom");
    const distanceToBottom = () => viewport.evaluate((el) => el.scrollHeight - el.clientHeight - el.scrollTop);
    await userScroll(viewport, 0);
    await expect(button).toBeVisible();
    await observeFor(page, 300, async () => {
      expect(await viewport.evaluate((el) => el.scrollTop)).toBe(0);
    });

    let jumpPosition: { top: number; max: number } | null = null;
    await page.exposeFunction("streamAfterJumpScroll", (top: number, max: number) => {
      jumpPosition = { top, max };
      mock.pushEvents([agentMessageChunk("\n\nOutput arriving during the jump.")]);
    });
    await viewport.evaluate((el) => {
      el.addEventListener(
        "scroll",
        () => {
          void (
            window as typeof window & {
              streamAfterJumpScroll: (top: number, max: number) => Promise<void>;
            }
          ).streamAfterJumpScroll(el.scrollTop, el.scrollHeight - el.clientHeight);
        },
        { once: true },
      );
    });
    await button.click();
    await expect(viewport).toContainText("Output arriving during the jump.");
    await test.info().attach("jump-scroll-position", {
      body: JSON.stringify(jumpPosition),
      contentType: "application/json",
    });
    await expect.poll(distanceToBottom).toBeLessThanOrEqual(1);
    mock.pushEvents([agentMessageChunk("\n\nLater output follows the jump.")]);
    await expect(viewport).toContainText("Later output follows the jump.");
    await expect.poll(distanceToBottom).toBeLessThanOrEqual(1);

    await userScroll(viewport, 0);
    await expect(button).toBeVisible();
    mock.pushEvents([agentMessageChunk("\n\nOutput after reader escape.")]);
    await expect(viewport).toContainText("Output after reader escape.");
    await observeFor(page, 300, async () => {
      expect(await viewport.evaluate((el) => el.scrollTop)).toBe(0);
    });
  });

  test("sticks to the bottom as content streams in, but never yanks a scrolled-up reader", async ({ page }) => {
    const mock = await mockAcpSession(page, {
      title: "story-stick",
      initialEvents: [
        agentMessageChunk(Array.from({ length: 120 }, (_, i) => `history line ${i}`).join("\n")),
        stopped(),
      ],
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

    // A reader who scrolled up is not yanked back down by new content.
    await userScroll(viewport, 0);
    await expect(page.getByTestId("acp-jump-to-bottom")).toBeVisible();
    const before = await viewport.evaluate((el) => el.scrollTop);
    mock.pushEvents([agentMessageChunk("\nlater\nlater\nlater\nlater")]);
    await expect(viewport).toContainText("later");
    const after = await viewport.evaluate((el) => el.scrollTop);
    expect(Math.abs(after - before)).toBeLessThan(4);
    await expect(page.getByTestId("acp-jump-to-bottom")).toBeVisible();
  });

  test("stays pinned across a composer collapse toggle and as the composer grows", async ({ page }) => {
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

    // A growing composer shrinks the transcript but keeps it pinned; typing is not a scroll gesture.
    const textarea = page.getByRole("textbox").first();
    const beforeHeight = await textarea.evaluate((el) => el.getBoundingClientRect().height);
    await textarea.fill(Array.from({ length: 8 }, (_, i) => `draft line ${i}`).join("\n"));
    await expect.poll(() => textarea.evaluate((el) => el.getBoundingClientRect().height)).toBeGreaterThan(beforeHeight);
    await expect.poll(isPinned).toBe(true);
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
});
