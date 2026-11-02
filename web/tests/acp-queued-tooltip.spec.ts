// The queued-messages header explains the Daemon-held queue in a tooltip that keyboard users can reach.

import { test, expect } from "./helpers/mockedTest";
import { mockAcpSession, openStructuredSession } from "./helpers/acpMock";

test("the queued header is reachable by keyboard and says the Daemon holds the queue", async ({ page }) => {
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
  await expect(header).toContainText("Queued (2)");
  await expect(header).not.toHaveAttribute("title");
  await expect(page.getByRole("tooltip")).toHaveCount(0);

  // Shift+Tab from "Clear all" lands on the header, which opens the tooltip.
  await page.getByRole("button", { name: "Clear all" }).focus();
  await page.keyboard.press("Shift+Tab");
  await expect(header).toBeFocused();
  await expect(page.getByRole("tooltip")).toHaveText("Sends when the Agent is free, even if you close this tab.");

  await page.keyboard.press("Tab");
  await expect(page.getByRole("tooltip")).toHaveCount(0);
});
