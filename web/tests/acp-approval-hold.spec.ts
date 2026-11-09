// Keyboard hold on destructive approvals in a real browser: a completed hold
// submits once, and every way the hold can end early submits nothing.

import type { Locator, Page } from "@playwright/test";
import { test, expect } from "./helpers/mockedTest";
import { mockAcpSession, openStructuredSession } from "./helpers/acpMock";

const HOLD_MS = 800;

function approval(choice: boolean) {
  return {
    nonce: "n-hold",
    tool_call: {
      id: "t-hold",
      name: "Bash",
      kind: "execute",
      args_preview: JSON.stringify({ command: "rm -rf build" }),
      started_at: new Date().toISOString(),
    },
    destructive: true,
    options: choice
      ? [
          { option_id: "o-yes", name: "Delete it", kind: "allow_once" },
          { option_id: "o-no", name: "Keep it", kind: "reject_once" },
        ]
      : [],
    choice,
    requested_at: new Date().toISOString(),
    resolved: null,
  };
}

const TARGETS = [
  { label: "Hold to allow", choice: false, body: { decision: "Allow" } },
  { label: "Delete it", choice: true, body: { decision: "Allow", option_id: "o-yes" } },
];

async function openHold(page: Page, choice: boolean) {
  await page.clock.install();
  const mock = await mockAcpSession(page, {
    title: "approval-hold",
    reducedState: { pending_approvals: [approval(choice)] },
  });
  const decisions: unknown[] = [];
  await page.route("**/api/sessions/*/acp/approvals/**", (r) => {
    decisions.push(r.request().postDataJSON());
    return r.fulfill({ json: {} });
  });
  await openStructuredSession(page, mock);
  await expect(page.getByRole("alertdialog")).toBeVisible({ timeout: 10_000 });
  await page.clock.pauseAt(new Date(Date.now() + 1000));
  return decisions;
}

const fill = (button: Locator) =>
  button.locator('span[aria-hidden="true"]').evaluate((n) => new DOMMatrix(getComputedStyle(n).transform).a);

for (const target of TARGETS) {
  for (const key of ["Space", "Enter"]) {
    test(`${target.label}: holding ${key} submits once and the release adds nothing`, async ({ page }) => {
      const decisions = await openHold(page, target.choice);
      const button = page.getByRole("button", { name: target.label, exact: true });
      await button.focus();
      await page.keyboard.down(key);
      await page.clock.runFor(HOLD_MS / 2);
      expect(await fill(button)).toBeGreaterThan(0.3);
      expect(decisions).toEqual([]);
      await page.clock.runFor(HOLD_MS);
      await expect.poll(() => decisions).toEqual([target.body]);
      await page.keyboard.up(key);
      await page.clock.runFor(HOLD_MS * 2);
      await expect(page.getByRole("alertdialog")).toHaveCount(0);
      expect(decisions).toEqual([target.body]);
    });
  }
}

const EARLY_ENDS: Array<[string, (page: Page) => Promise<void>]> = [
  ["releasing the key", (page) => page.keyboard.up("Space")],
  ["moving focus", (page) => page.keyboard.press("Tab")],
  ["the window losing focus", (page) => page.evaluate(() => window.dispatchEvent(new Event("blur")))],
  [
    "the page being hidden",
    (page) =>
      page.evaluate(() => {
        Object.defineProperty(document, "hidden", { configurable: true, get: () => true });
        document.dispatchEvent(new Event("visibilitychange"));
      }),
  ],
];

for (const target of TARGETS) {
  for (const [how, end] of EARLY_ENDS) {
    test(`${target.label}: ${how} before the deadline submits nothing`, async ({ page }) => {
      const decisions = await openHold(page, target.choice);
      const button = page.getByRole("button", { name: target.label, exact: true });
      await button.focus();
      await page.keyboard.down("Space");
      await page.clock.runFor(HOLD_MS / 2);
      expect(await fill(button)).toBeGreaterThan(0.3);
      await end(page);
      await page.clock.runFor(HOLD_MS * 2);
      expect(decisions).toEqual([]);
      expect(await fill(button)).toBe(0);
      await page.keyboard.up("Space");
      await page.clock.runFor(HOLD_MS);
      await expect(button).toBeEnabled();
      expect(decisions).toEqual([]);
    });
  }
}
