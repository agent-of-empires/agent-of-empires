// A new Structured view Session that is still starting: one line names the Agent and counts up.

import { test, expect } from "./helpers/mockedTest";
import { mockAcpSession, openStructuredSession } from "./helpers/acpMock";

async function openStarting(page: import("@playwright/test").Page) {
  const mock = await mockAcpSession(page, { title: "story-starting", workerStarting: true });
  await openStructuredSession(page, mock);
  const line = page.getByTestId("startup-wait-line");
  await expect(line).toContainText("Starting Claude Code… You can type now.");
  return line;
}

test("one startup line names the Agent and counts", async ({ page }) => {
  const line = await openStarting(page);
  await expect(page.getByTestId("startup-wait-line")).toHaveCount(1);
  await expect(page.getByText(/structured view worker|few seconds/)).toHaveCount(0);
  await expect(line.getByTestId("startup-wait-elapsed")).toHaveText(/^0:0[0-9]$/);
});

test.describe("at 375px", () => {
  test.use({ viewport: { width: 375, height: 812 } });

  test("the line fits without horizontal scroll and keeps the count visible", async ({ page }) => {
    const line = await openStarting(page);
    await expect(line.getByTestId("startup-wait-elapsed")).toBeInViewport();
    // Nothing overlays the count, such as the header collapse toggle.
    const box = (await line.getByTestId("startup-wait-elapsed").boundingBox())!;
    for (const x of [box.x + box.width / 2, box.x + box.width - 1]) {
      const on = await page.evaluate(
        ([x, y]) => document.elementFromPoint(x!, y!)?.closest("[data-testid]")?.getAttribute("data-testid"),
        [x, box.y + 2],
      );
      expect(on).toBe("startup-wait-elapsed");
    }
    const overflow = await page.evaluate(
      () => document.documentElement.scrollWidth - document.documentElement.clientWidth,
    );
    expect(overflow).toBeLessThanOrEqual(0);
  });
});
