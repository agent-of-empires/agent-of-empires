// The adapter compatibility screen: the setting unlocks Update on focus, and the phone layout.

import type { Page } from "@playwright/test";

import { test, expect } from "./helpers/mockedTest";
import { mockAcpSession, openStructuredSession } from "./helpers/acpMock";

const IN_CONTAINER = "podman exec aoe-sandbox-story-ad npm install -g @agentclientprotocol/claude-agent-acp@latest";

/** A sandboxed Session refused for an old adapter; `allow` is what install-info reports next. */
async function openRefusedSession(page: Page) {
  const mock = await mockAcpSession(page, {
    title: "story-adapter-out-of-date",
    nativeAvailable: false,
    initialEvents: [
      {
        IncompatibleAgent: {
          detail: {
            kind: "incompatible_agent_version",
            package_name: "@agentclientprotocol/claude-agent-acp",
            installed: "0.55.0",
            required: "0.82.0",
            install_command: "npm install -g @agentclientprotocol/claude-agent-acp@latest",
            auto_install: true,
          },
        },
      },
    ],
  });
  const settings = { allow: false };
  await page.route("**/api/sessions/*/acp/install-info", (r) =>
    r.fulfill({
      json: {
        agent: "claude",
        display_name: "Claude",
        install_command: "npm install -g @agentclientprotocol/claude-agent-acp@latest",
        npm_installable: true,
        install_allowed: settings.allow,
        install_blocked: settings.allow ? null : "setting_off",
        sandboxed: true,
        container_install_command: IN_CONTAINER,
      },
    }),
  );
  await openStructuredSession(page, mock);
  const screen = page.getByTestId("startup-error-screen");
  await expect(screen).toBeVisible();
  return { screen, settings };
}

test("turning the setting on unlocks Update when the page regains focus", async ({ page }) => {
  const { screen, settings } = await openRefusedSession(page);
  const update = screen.getByRole("button", { name: "Update in Sandbox & restart" });
  await expect(update).toBeDisabled();
  await expect(update).toHaveAccessibleDescription(/Allow installs from the Web dashboard/);

  settings.allow = true;
  await page.evaluate(() => window.dispatchEvent(new Event("focus")));
  await expect(update).toBeEnabled();
  await expect(screen.getByTestId("startup-error-step-1")).toContainText("Done");
});

test.describe("at 375px", () => {
  test.use({ viewport: { width: 375, height: 812 } });

  test("the buttons wrap and the command scrolls inside its block", async ({ page }) => {
    const { screen } = await openRefusedSession(page);
    const update = screen.getByRole("button", { name: "Update in Sandbox & restart" });
    const check = screen.getByRole("button", { name: "Check again" });
    const updateBox = (await update.boundingBox())!;
    const checkBox = (await check.boundingBox())!;
    expect(checkBox.y).toBeGreaterThan(updateBox.y);

    const command = screen.getByTestId("startup-error-install-command");
    await expect(command).toHaveText(IN_CONTAINER);
    const scrolls = await command.evaluate((el) => el.scrollWidth > el.clientWidth);
    expect(scrolls).toBe(true);
    const overflow = await page.evaluate(
      () => document.documentElement.scrollWidth - document.documentElement.clientWidth,
    );
    expect(overflow).toBeLessThanOrEqual(0);
  });
});
