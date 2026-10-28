import { expect, type Page } from "@playwright/test";

/**
 * Clears the clipboard, runs `copy`, and returns what lands there. The write is asynchronous, so a read right
 * after a click can return the previous content; clearing first means a copy that writes nothing fails here
 * instead of passing on stale text, and the poll waits for the new content.
 */
export async function copiedBy(page: Page, copy: () => Promise<unknown>): Promise<string> {
  await page.evaluate(() => navigator.clipboard.writeText(""));
  await copy();
  await expect.poll(() => page.evaluate(() => navigator.clipboard.readText())).not.toBe("");
  return page.evaluate(() => navigator.clipboard.readText());
}
