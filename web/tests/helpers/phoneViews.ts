import { expect, type Page } from "@playwright/test";

/** Pick one of the open Session's views from the phone header's ⋯, by its menu label. */
export async function pickPhoneView(page: Page, label: "Agent" | "Terminal" | "Diff" | "Files" | "More panes…") {
  // The header's ⋯; the session drawer carries the app's own.
  await page.locator("header").getByRole("button", { name: "More options" }).click();
  await page.getByRole("menuitemradio", { name: label }).click();
}

/** The phone shows `label`'s view: the agent, or another view the title pill names. */
export async function expectPhoneView(page: Page, label: string) {
  const pill = page.getByTestId("session-title-pill");
  if (label === "Agent") await expect(pill).not.toContainText("·");
  else await expect(pill).toContainText(`${label} ·`);
}
