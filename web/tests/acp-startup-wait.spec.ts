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

  test("a long unspaced Agent name wraps in the line and the queued heading", async ({ page }) => {
    // Underscores, unlike hyphens, give the browser no place to break.
    const agent = "acme_internal_structured_agent_with_a_very_long_registry_name";
    const mock = await mockAcpSession(page, { title: "story-long-agent", workerStarting: true, startingAgent: agent });
    // Two prompts the Daemon already holds for the starting Session.
    const at = new Date().toISOString();
    await page.route("**/api/sessions/*/queue", (r) =>
      r.fulfill({
        json: [
          { id: "q1", seq: 1, text: "first", created_at: at },
          { id: "q2", seq: 2, text: "second", created_at: at },
        ],
      }),
    );
    await openStructuredSession(page, mock);
    const line = page.getByTestId("startup-wait-line");
    await expect(line).toContainText(`Starting ${agent}…`);
    await expect(line.getByTestId("startup-wait-elapsed")).toBeInViewport({ ratio: 1 });
    expect(await line.evaluate((el) => el.scrollWidth - el.clientWidth)).toBeLessThanOrEqual(0);

    const heading = page.getByText(`Pending until ${agent} is ready (2)`);
    await expect(heading).toBeVisible();
    const clearAll = page.getByRole("button", { name: "Clear all" });
    await expect(clearAll).toBeInViewport({ ratio: 1 });
    const headingBox = (await heading.boundingBox())!;
    expect(headingBox.x + headingBox.width).toBeLessThanOrEqual((await clearAll.boundingBox())!.x);
  });
});
