// User story (#3916): on a phone-width viewport the composer shows
// context-window usage and session spend.

import { test, expect } from "./helpers/mockedTest";
import { mockAcpSession, openStructuredSession, usageUpdated } from "./helpers/acpMock";

test.use({ viewport: { width: 360, height: 740 } });

test("mobile composer shows a compact usage hint inside the viewport", async ({ page }) => {
  const mock = await mockAcpSession(page, {
    title: "story-usage-mobile",
    initialEvents: [usageUpdated({ used: 120_000, size: 200_000, cost: { amount: 0.42, currency: "USD" } })],
  });
  await openStructuredSession(page, mock);

  const usage = page.getByTestId("composer-usage");
  await expect(usage).toBeVisible({ timeout: 15_000 });
  await expect(usage).toHaveAccessibleName(/Context window: .* tokens used \(60%\)/);
  await expect(usage).toContainText("60%");
  await expect(usage).toContainText("0.42");
  // Compact variant: token counts are desktop-only.
  await expect(usage.getByText("120k/200k")).toBeHidden();

  const box = await usage.boundingBox();
  expect(box).not.toBeNull();
  expect(box!.x).toBeGreaterThanOrEqual(0);
  expect(box!.x + box!.width).toBeLessThanOrEqual(page.viewportSize()!.width);
});
