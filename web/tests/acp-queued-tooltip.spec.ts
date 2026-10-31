// The queued-messages header's tooltip opens on keyboard focus and dismisses on Tab.

import { test, expect } from "./helpers/mockedTest";
import { mockAcpSession, openStructuredSession } from "./helpers/acpMock";

test("the queued header tooltip opens on keyboard focus and dismisses on Tab", async ({ page }) => {
  const mock = await mockAcpSession(page, { title: "story-queued" });
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
  const header = page.getByTestId("queued-strip-header");
  await expect(header).toBeVisible();
  await expect(page.getByRole("tooltip")).toHaveCount(0);

  // Shift+Tab from "Clear all" lands on the header, which opens the tooltip.
  await page.getByRole("button", { name: "Clear all" }).focus();
  await page.keyboard.press("Shift+Tab");
  await expect(header).toBeFocused();
  await expect(page.getByRole("tooltip")).toBeVisible();

  await page.keyboard.press("Tab");
  await expect(page.getByRole("tooltip")).toHaveCount(0);
});
