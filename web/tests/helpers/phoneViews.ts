import { expect, type Page } from "@playwright/test";

/** Pick one of the open Session's views from the phone header's ⋯, by its menu label. */
export async function pickPhoneView(page: Page, label: "Agent" | "Terminal" | "Diff" | "Files" | "More panes…") {
  // The header's ⋯; the session drawer carries the app's own.
  await page.locator("header").getByRole("button", { name: "More options" }).click();
  await page.getByRole("menuitemradio", { name: label }).click();
}

/** The phone shows `label`'s view: the agent, or another view the title pill names. */
export async function expectPhoneView(page: Page, label: string) {
  const status = page.getByTestId("session-title-status");
  // The agent's pill says what the Session is doing; another view names itself.
  if (label === "Agent") await expect(status).not.toHaveText(/^(Terminal|Diff|Files|Background) ·/);
  else await expect(status).toHaveText(new RegExp(`^${label} ·`));
}
