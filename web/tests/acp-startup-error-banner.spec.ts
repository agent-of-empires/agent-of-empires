// The Structured view startup error banner's actions, keyboard order and phone layout.

import type { Page } from "@playwright/test";

import { test, expect } from "./helpers/mockedTest";
import { mockAcpSession, openStructuredSession } from "./helpers/acpMock";

const MESSAGE =
  'Failed to start structured view agent "claude": acp client error: agent spawn failed: runner closed before Hello';

/** A Structured view Session whose agent failed to start, with install from the dashboard off. */
async function openFailedSession(page: Page) {
  const mock = await mockAcpSession(page, {
    title: "story-startup-error",
    initialEvents: [{ AgentStartupError: { message: MESSAGE } }],
  });
  const disables: string[] = [];
  await page.route("**/api/sessions/*/acp/disable", (r) => {
    disables.push(r.request().url());
    return r.fulfill({ json: {} });
  });
  await page.route("**/api/sessions/*/acp/install-info", (r) =>
    r.fulfill({
      json: {
        agent: "claude",
        display_name: "Claude",
        install_command: "npm install -g @agentclientprotocol/claude-agent-acp@latest",
        npm_installable: true,
        install_allowed: false,
        sandboxed: false,
      },
    }),
  );
  await openStructuredSession(page, mock);
  const banner = page.getByTestId("startup-error-banner");
  await expect(banner.getByRole("heading", { name: "Claude couldn't start in Structured view" })).toBeVisible();
  await expect(banner.getByTestId("startup-error-banner-command")).toBeVisible();
  return { banner, disables };
}

test("Use Terminal view confirms through the switch-view dialog, in keyboard order", async ({ page }) => {
  const { banner, disables } = await openFailedSession(page);

  await banner.getByRole("button", { name: "Copy" }).focus();
  for (const next of ["Retry", "Use Terminal view", "Details"]) {
    await page.keyboard.press("Tab");
    await expect(banner.getByRole("button", { name: next })).toBeFocused();
  }

  await page.keyboard.press("Shift+Tab");
  await page.keyboard.press("Enter");
  const dialog = page.getByTestId("switch-view-dialog");
  await expect(dialog).toBeVisible();
  await expect(dialog).toContainText("Switch this session back to a tmux terminal?");
  // The banner only opens the confirmation; the switch waits for it.
  expect(disables).toEqual([]);
  await dialog.getByRole("button", { name: "Cancel" }).click();
  await expect(dialog).toBeHidden();
  expect(disables).toEqual([]);
});

test.describe("at 375px", () => {
  test.use({ viewport: { width: 375, height: 812 } });

  test("the actions wrap with a full-width primary and no horizontal scroll", async ({ page }) => {
    const { banner } = await openFailedSession(page);
    const retry = banner.getByRole("button", { name: "Retry" });
    const terminal = banner.getByRole("button", { name: "Use Terminal view" });
    const bannerBox = (await banner.boundingBox())!;
    const retryBox = (await retry.boundingBox())!;
    const terminalBox = (await terminal.boundingBox())!;

    // Full width inside the banner's 16px gutters.
    expect(retryBox.width).toBeGreaterThan(bannerBox.width - 40);
    expect(terminalBox.y).toBeGreaterThan(retryBox.y);
    for (const box of [retryBox, terminalBox]) {
      expect(box.height).toBeGreaterThanOrEqual(32);
      expect(box.height).toBeLessThanOrEqual(40);
    }
    const overflow = await page.evaluate(
      () => document.documentElement.scrollWidth - document.documentElement.clientWidth,
    );
    expect(overflow).toBeLessThanOrEqual(0);
  });
});
