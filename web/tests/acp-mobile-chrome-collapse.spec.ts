import { test, expect } from "./helpers/mockedTest";
import { devices } from "@playwright/test";
import { agentMessageChunk, mockAcpSession, openStructuredSession, waitForComposerConnected } from "./helpers/acpMock";

// On phones the top bar and composer each fold away behind a handle that stays tappable in both states.
test.use({ ...devices["iPhone 13"] });

test.describe("mobile conversation chrome collapse", () => {
  test("header and composer collapse independently and hand their height to the transcript", async ({ page }) => {
    const mock = await mockAcpSession(page, {
      title: "story-collapse",
      initialEvents: [agentMessageChunk("hello from the agent")],
    });
    await openStructuredSession(page, mock);
    await waitForComposerConnected(page);

    const headerToggle = page.getByTestId("header-collapse-toggle");
    const composerToggle = page.getByTestId("composer-collapse-toggle");

    // Measure heights: a clipped child still reports visible, but the row must release its height.
    const heightOf = async (testId: string) => (await page.getByTestId(testId).boundingBox())!.height;
    const viewportHeight = () => heightOf("acp-viewport");

    await expect(page.getByTestId("composer-footer")).toBeVisible();

    // The draft must survive collapsing; typed before the baseline because the textarea sizes to content.
    const draft = page.getByRole("textbox").first();
    await draft.fill("half-written prompt");

    const headerHeight = await heightOf("conversation-header");
    const composerHeight = await heightOf("conversation-composer");
    expect(headerHeight).toBeGreaterThan(0);
    expect(composerHeight).toBeGreaterThan(0);
    const bothExpanded = await viewportHeight();

    await expect(composerToggle).toHaveAttribute("aria-label", "Collapse message composer");
    await composerToggle.click();
    await expect.poll(() => heightOf("conversation-composer")).toBe(0);
    expect(await heightOf("conversation-header")).toBe(headerHeight);
    await expect(composerToggle).toHaveAttribute("aria-label", "Expand message composer");
    // A collapsed composer is inert: taps and focus() miss it. It is off the accessibility tree, so query the DOM.
    await page.mouse.click(150, 250);
    const focusedAfter = await page.evaluate(() => {
      document.querySelector<HTMLTextAreaElement>("textarea")?.focus();
      return document.activeElement?.tagName ?? null;
    });
    expect(focusedAfter).not.toBe("TEXTAREA");
    const composerOnly = await viewportHeight();
    expect(composerOnly).toBeCloseTo(bothExpanded + composerHeight, 0);

    await headerToggle.click();
    await expect.poll(() => heightOf("conversation-header")).toBe(0);
    await expect(headerToggle).toBeVisible();
    await expect(composerToggle).toBeVisible();
    const bothCollapsed = await viewportHeight();
    expect(bothCollapsed).toBeCloseTo(composerOnly + headerHeight, 0);

    await composerToggle.click();
    await expect.poll(() => heightOf("conversation-composer")).toBe(composerHeight);
    expect(await heightOf("conversation-header")).toBe(0);
    expect(await viewportHeight()).toBeLessThan(bothCollapsed);

    await headerToggle.click();
    await expect.poll(() => heightOf("conversation-header")).toBe(headerHeight);
    expect(await viewportHeight()).toBeCloseTo(bothExpanded, 0);

    await expect(draft).toHaveValue("half-written prompt");
    await draft.fill("still typing");
    await page.getByRole("button", { name: "Send message" }).click();
    await expect.poll(() => mock.promptBodies.map((b) => b.text)).toContain("still typing");
  });

  // The handles overlay content, so their hit area must equal the painted tab; a larger wrapper once ate the update banner's dismiss taps.
  test("each handle's clickable area is the tab you can see, so it intercepts nothing around it", async ({ page }) => {
    const mock = await mockAcpSession(page, {
      title: "story-collapse-hit",
      initialEvents: [agentMessageChunk("hello from the agent")],
    });
    await page.route("**/api/system/update-status", (r) =>
      r.fulfill({
        json: {
          update_check_mode: "notify",
          current_version: "0.5.0",
          latest_version: "0.6.0",
          update_available: true,
          release_url: "https://example.invalid/releases/v0.6.0",
          error: null,
        },
      }),
    );
    await openStructuredSession(page, mock);
    await waitForComposerConnected(page);

    const handleAt = (x: number, y: number) =>
      page.evaluate(
        ([px, py]) =>
          document
            .elementFromPoint(px, py)
            ?.closest<HTMLElement>("[data-testid$='-collapse-toggle']")
            ?.getAttribute("data-testid") ?? null,
        [x, y],
      );

    for (const testId of ["header-collapse-toggle", "composer-collapse-toggle"]) {
      const box = (await page.getByTestId(testId).boundingBox())!;
      expect({ width: box.width, height: box.height }).toEqual({ width: 28, height: 16 });
      // The painted element is the clickable one, so a transparent wrapper cannot pass.
      const painted = await page
        .getByTestId(testId)
        .evaluate((el) => getComputedStyle(el).backgroundColor !== "rgba(0, 0, 0, 0)");
      expect(painted, `${testId} paints its own hit area`).toBe(true);
      expect(await handleAt(box.x + box.width / 2, box.y + box.height / 2)).toBe(testId);
      const outside = [
        [box.x + box.width / 2, box.y - 6],
        [box.x + box.width / 2, box.y + box.height + 6],
        [box.x - 6, box.y + box.height / 2],
        [box.x + box.width + 6, box.y + box.height / 2],
      ] as const;
      for (const [x, y] of outside) {
        expect(await handleAt(x, y), `${testId} at ${x},${y}`).toBeNull();
      }
    }

    // A real click proves nothing covers the banner's dismiss control in the same corner.
    const dismiss = page.getByRole("button", { name: "Dismiss update notice" });
    await dismiss.click({ timeout: 5_000 });
    await expect(page.getByRole("status", { name: /Update available/i })).toHaveCount(0);
  });

  // Phone-only: crossing to desktop restores both regions without losing the draft.
  test("desktop renders no handles, and crossing the breakpoint restores the chrome", async ({ page }) => {
    const mock = await mockAcpSession(page, {
      title: "story-collapse-desktop",
      initialEvents: [agentMessageChunk("hello from the agent")],
    });
    await openStructuredSession(page, mock);
    await waitForComposerConnected(page);

    const headerToggle = page.getByTestId("header-collapse-toggle");
    const composerToggle = page.getByTestId("composer-collapse-toggle");
    const heightOf = async (testId: string) => (await page.getByTestId(testId).boundingBox())!.height;

    await page.getByRole("textbox").first().fill("typed on the phone");
    const draftValue = () => page.evaluate(() => document.querySelector("textarea")?.value ?? null);
    await headerToggle.click();
    await composerToggle.click();
    await expect.poll(() => heightOf("conversation-header")).toBe(0);
    await expect.poll(() => heightOf("conversation-composer")).toBe(0);

    await page.setViewportSize({ width: 1280, height: 800 });
    await expect(headerToggle).toHaveCount(0);
    await expect(composerToggle).toHaveCount(0);
    await expect.poll(() => heightOf("conversation-header")).toBeGreaterThan(0);
    await expect.poll(() => heightOf("conversation-composer")).toBeGreaterThan(0);
    await expect(page.getByRole("textbox").first()).toHaveValue("typed on the phone");

    // Back on a phone: header collapse lives in App and persists, but the structured view remounts, so the composer returns expanded.
    await page.setViewportSize({ width: 390, height: 664 });
    await expect(headerToggle).toBeVisible();
    await expect(composerToggle).toBeVisible();
    await expect.poll(() => heightOf("conversation-header")).toBe(0);
    await expect.poll(() => heightOf("conversation-composer")).toBeGreaterThan(0);
    expect(await draftValue()).toBe("typed on the phone");
  });
});
