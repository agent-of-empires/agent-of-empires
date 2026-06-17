// Shared single-screen new-session wizard DSL (#2210).
//
// The wizard is one screen now: project picker + agent picker +
// structured-view toggle are always visible, everything else folds behind
// "More options", and a single Launch button submits. These helpers replace
// the old per-spec `openReviewStep` / `Next`-clicking walkers so every spec
// drives the new flow the same way. Works for both the mocked and live
// suites (plain `@playwright/test` Page).
//
// All control queries are scoped to the wizard modal via its
// `data-testid="session-wizard"` root, because the app shell behind the
// modal owns colliding labels (a topbar "More options" menu, a sidebar
// "New session" button).

import { expect, type Locator, type Page } from "@playwright/test";

/** The wizard modal root. Scope control queries to it to avoid colliding
 *  with the app shell behind the modal. */
export function wizard(page: Page): Locator {
  return page.getByTestId("session-wizard");
}

/** Open the wizard with the `n` keyboard shortcut and wait for it to mount. */
export async function openWizard(page: Page) {
  await page.locator("body").click();
  await page.keyboard.press("n");
  await expect(page.getByTestId("session-wizard")).toBeVisible();
}

/** Pick a recent/saved project by a substring of its path or display name. */
export async function selectProject(page: Page, pathText: string) {
  const recent = wizard(page).getByRole("button").filter({ hasText: pathText }).first();
  await recent.waitFor({ state: "visible", timeout: 5000 });
  await recent.click();
}

/** Pick an agent from the always-visible picker grid. */
export async function selectAgent(page: Page, name: string | RegExp) {
  await wizard(page).getByRole("button", { name }).click();
}

/** Expand the "More options" fold if it is not already open. Idempotent. */
export async function expandMoreOptions(page: Page) {
  const button = wizard(page).getByRole("button", { name: "More options" });
  await button.waitFor({ state: "visible" });
  if ((await button.getAttribute("aria-expanded")) !== "true") {
    await button.click();
  }
}

/** Fill the always-visible session title essential. */
export async function setTitle(page: Page, title: string) {
  await wizard(page).getByPlaceholder("Auto-generated if empty").fill(title);
}

/** Click the Launch button. */
export async function launch(page: Page) {
  await wizard(page)
    .getByRole("button", { name: /Launch session/ })
    .click();
}
