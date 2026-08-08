import { test, expect } from "./helpers/mockedTest";
import { devices } from "@playwright/test";
import { agentMessageChunk, mockAcpSession, openStructuredSession, waitForComposerConnected } from "./helpers/acpMock";

// User story: on a phone the top bar and the composer eat a third of the
// screen. Each gets its own handle that folds it away and hands the freed
// height to the transcript; the handles stay tappable in both states so the
// user is never stranded in a collapsed layout.
//
// Mocked (not live) because everything asserted here is client-side layout:
// no backend state is involved. One test walks all four combinations rather
// than four tests, per the repo's "one test per behavior" rule.
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

    // Heights, not visibility: a collapsed region clips a child that still has
    // a box of its own, so Playwright would report the child as visible. The
    // contract is that the *row* releases its layout height.
    const heightOf = async (testId: string) => (await page.getByTestId(testId).boundingBox())!.height;
    const viewportHeight = () => heightOf("acp-viewport");

    await expect(page.getByTestId("composer-footer")).toBeVisible();
    const headerHeight = await heightOf("conversation-header");
    const composerHeight = await heightOf("conversation-composer");
    expect(headerHeight).toBeGreaterThan(0);
    expect(composerHeight).toBeGreaterThan(0);
    const bothExpanded = await viewportHeight();

    // Composer collapsed, header still expanded.
    await expect(composerToggle).toHaveAttribute("aria-label", "Collapse message composer");
    await composerToggle.click();
    await expect.poll(() => heightOf("conversation-composer")).toBe(0);
    expect(await heightOf("conversation-header")).toBe(headerHeight);
    await expect(composerToggle).toHaveAttribute("aria-label", "Expand message composer");
    const composerOnly = await viewportHeight();
    expect(composerOnly).toBeCloseTo(bothExpanded + composerHeight, 0);

    // Both collapsed: the handles are still on screen and tappable.
    await headerToggle.click();
    await expect.poll(() => heightOf("conversation-header")).toBe(0);
    await expect(headerToggle).toBeVisible();
    await expect(composerToggle).toBeVisible();
    const bothCollapsed = await viewportHeight();
    expect(bothCollapsed).toBeCloseTo(composerOnly + headerHeight, 0);

    // Header collapsed, composer restored: the two states are independent.
    await composerToggle.click();
    await expect.poll(() => heightOf("conversation-composer")).toBe(composerHeight);
    expect(await heightOf("conversation-header")).toBe(0);
    expect(await viewportHeight()).toBeLessThan(bothCollapsed);

    // Back to both expanded, and the composer still works after the round trip.
    await headerToggle.click();
    await expect.poll(() => heightOf("conversation-header")).toBe(headerHeight);
    expect(await viewportHeight()).toBeCloseTo(bothExpanded, 0);

    await page.getByRole("textbox").first().fill("still typing");
    await page.getByRole("button", { name: "Send message" }).click();
    await expect.poll(() => mock.promptBodies.map((b) => b.text)).toContain("still typing");
  });
});
