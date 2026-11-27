import { expect } from "@playwright/test";
import type { Page } from "@playwright/test";

/** Opens a review pane from the header's pane menu, leaving it alone if it already shows. */
export async function openReviewPane(page: Page, id: string) {
  const chevron = page.getByTestId("pane-menu-toggle");
  await expect(chevron).toBeVisible();
  await chevron.click();
  const item = page.getByTestId(`pane-toggle-${id}`);
  if ((await item.getAttribute("aria-checked")) === "true") await page.keyboard.press("Escape");
  else await item.click();
}
