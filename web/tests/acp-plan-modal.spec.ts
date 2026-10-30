// Plan modal on an ExitPlanMode approval: focus entry, Tab containment, Escape
// and focus restore, and the hand-off to the file viewer when a plan link opens
// a file.

import type { Page } from "@playwright/test";
import { test, expect } from "./helpers/mockedTest";
import { mockAcpSession, openStructuredSession } from "./helpers/acpMock";

const TITLE = "plan-modal";
const PLAN = `# Plan\n\nRead [PLAN.md](/tmp/${TITLE}/PLAN.md) first.\n\n- step one\n- step two`;

const approval = {
  nonce: "n-plan",
  tool_call: {
    id: "t-plan",
    name: "Ready to code?",
    kind: "switch_mode",
    args_preview: JSON.stringify({ plan: PLAN }),
    started_at: new Date().toISOString(),
  },
  destructive: false,
  options: [],
  choice: false,
  plan: PLAN,
  requested_at: new Date().toISOString(),
  resolved: null,
};

async function openPlanModal(page: Page) {
  const mock = await mockAcpSession(page, { title: TITLE, reducedState: { pending_approvals: [approval] } });
  await openStructuredSession(page, mock);
  const opener = page.getByRole("button", { name: "View plan" });
  await expect(opener).toBeVisible({ timeout: 10_000 });
  await opener.click();
  const dialog = page.getByRole("dialog", { name: "Plan" });
  await expect(dialog).toBeVisible();
  return { opener, dialog };
}

test("plan modal takes focus, contains Tab, and returns focus on Escape", async ({ page }) => {
  const { opener, dialog } = await openPlanModal(page);
  const insideDialog = () => dialog.evaluate((el) => el.contains(document.activeElement));

  await expect.poll(insideDialog).toBe(true);
  for (let i = 0; i < 12; i++) {
    await page.keyboard.press("Tab");
    expect(await insideDialog()).toBe(true);
  }
  for (let i = 0; i < 12; i++) {
    await page.keyboard.press("Shift+Tab");
    expect(await insideDialog()).toBe(true);
  }

  await page.keyboard.press("Escape");
  await expect(dialog).toBeHidden();
  await expect(opener).toBeFocused();
});

test("a plan file link closes the modal and leaves Tab alone while the approval stays pending", async ({ page }) => {
  const { dialog } = await openPlanModal(page);

  await dialog.getByRole("link", { name: "PLAN.md" }).click();
  // The structured stack is hidden, not unmounted, so a role query would pass for a live modal.
  await expect(page.getByTestId("plan-modal")).toHaveCount(0);

  await page.evaluate(() => {
    const seen: boolean[] = [];
    (window as unknown as { __tabPrevented: boolean[] }).__tabPrevented = seen;
    window.addEventListener("keydown", (e) => {
      if (e.key === "Tab") seen.push(e.defaultPrevented);
    });
  });
  await page.keyboard.press("Tab");
  expect(await page.evaluate(() => (window as unknown as { __tabPrevented: boolean[] }).__tabPrevented)).toEqual([
    false,
  ]);

  await expect(page.getByRole("alertdialog", { includeHidden: true })).toHaveCount(1);
});
