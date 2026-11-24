// The adapter compatibility screen: the setting unlocks Update on focus, the update while it runs and when it fails,
// and the phone layout with Recreate container.

import type { Page } from "@playwright/test";

import { test, expect } from "./helpers/mockedTest";
import { mockAcpSession, openStructuredSession } from "./helpers/acpMock";

const IN_CONTAINER = "podman exec aoe-sandbox-story-ad npm install -g @agentclientprotocol/claude-agent-acp@latest";

/** A sandboxed Session refused for an old adapter; `allow` is what install-info reports next. */
async function openRefusedSession(page: Page, allow = false) {
  const mock = await mockAcpSession(page, {
    title: "story-adapter-out-of-date",
    sessionFields: { is_sandboxed: true },
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
  const settings = { allow };
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
  await expect(screen.getByRole("heading", { name: "Claude needs an update in this sandbox" })).toBeVisible();
  return { screen, settings };
}

/** Copy reads as a button: a border and a fill, not bare text. */
async function expectCopyIsAButton(screen: ReturnType<Page["getByTestId"]>) {
  const copy = screen.getByRole("button", { name: "Copy" });
  const style = await copy.evaluate((el) => {
    const s = getComputedStyle(el);
    return { border: parseFloat(s.borderTopWidth), fill: s.backgroundColor };
  });
  expect(style.border).toBeGreaterThan(0);
  expect(style.fill).not.toBe("rgba(0, 0, 0, 0)");
}

async function expectNoSidewaysScroll(page: Page) {
  const overflow = await page.evaluate(
    () => document.documentElement.scrollWidth - document.documentElement.clientWidth,
  );
  expect(overflow).toBeLessThanOrEqual(0);
}

/** Holds install-agent until `answer` is called with npm's result. */
async function holdInstall(page: Page) {
  let answer: (body: object) => void = () => {};
  const answered = new Promise<object>((r) => (answer = r));
  let started = false;
  await page.route("**/api/sessions/*/acp/install-agent", async (r) => {
    started = true;
    await r.fulfill({ json: await answered });
  });
  return { answer, started: () => started };
}

const NPM_FAILED = {
  session_id: "s",
  package: "@agentclientprotocol/claude-agent-acp@latest",
  success: false,
  exit_code: 1,
  stdout: "",
  stderr: "npm ERR! code EACCES",
  recovered_sessions: 0,
};

for (const viewport of [
  { label: "desktop", width: 1280, height: 800 },
  { label: "375px", width: 375, height: 812 },
]) {
  test.describe(`the update at ${viewport.label}`, () => {
    test.use({ viewport: { width: viewport.width, height: viewport.height } });

    test("while it runs only the update shows, and a failure gets its own card", async ({ page }) => {
      const { screen } = await openRefusedSession(page, true);
      const install = await holdInstall(page);
      await screen.getByRole("button", { name: "Update & restart" }).click();
      await expect.poll(install.started).toBe(true);

      await expect(screen.getByRole("button", { name: "Updating…" })).toBeDisabled();
      await expect(screen.getByRole("button", { name: "Check again" })).toBeDisabled();
      await expect(screen.getByTestId("startup-error-step-2")).toHaveText(
        "2Updating inside the containerClaude restarts when it's done.",
      );
      await expect(screen.getByTestId("startup-error-install-command")).toHaveCount(0);
      await expect(screen.getByText(/then Check again/)).toHaveCount(0);
      await expectNoSidewaysScroll(page);

      install.answer(NPM_FAILED);
      await expect(screen.getByRole("heading", { name: "Claude couldn't be updated in this sandbox" })).toBeVisible();
      await expect(screen.getByText("Update failed")).toBeVisible();
      await expect(screen.getByTestId("startup-error-version")).toHaveText("npm exited with code 1");
      await expect(screen.getByTestId("startup-error-steps")).toHaveCount(0);
      const command = screen.getByTestId("startup-error-install-command");
      await expect(command).toHaveText(IN_CONTAINER);
      await expect(command).toBeInViewport({ ratio: 1 });
      await expectCopyIsAButton(screen);
      const retry = screen.getByRole("button", { name: "Try again" });
      await expect(retry).toBeEnabled();
      await expect(retry).toBeInViewport({ ratio: 1 });
      await expect(screen.getByText("npm ERR! code EACCES")).toHaveCount(0);
      await screen.getByRole("button", { name: "Details" }).click();
      await expect(screen.getByTestId("startup-error-install-output")).toHaveText("npm ERR! code EACCES");
      await expectNoSidewaysScroll(page);
    });
  });
}

test("turning the setting on unlocks Update when the page regains focus", async ({ page }) => {
  const { screen, settings } = await openRefusedSession(page);
  const update = screen.getByRole("button", { name: "Update & restart" });
  await expect(update).toBeDisabled();
  await expect(update).toHaveAccessibleDescription(/Allow installs from the Web dashboard/);

  settings.allow = true;
  await page.evaluate(() => window.dispatchEvent(new Event("focus")));
  await expect(update).toBeEnabled();
  await expect(screen.getByTestId("startup-error-step-1")).toHaveText("✓Installs from the Web dashboard are allowed");
  await expect(screen.getByRole("button", { name: "Recreate container" })).toBeVisible();
});

test.describe("at 375px", () => {
  test.use({ viewport: { width: 375, height: 812 } });

  test("the buttons wrap and the command wraps inside its block", async ({ page }) => {
    const { screen } = await openRefusedSession(page);
    const update = screen.getByRole("button", { name: "Update & restart" });
    const check = screen.getByRole("button", { name: "Check again" });
    const updateBox = (await update.boundingBox())!;
    const checkBox = (await check.boundingBox())!;
    expect(checkBox.y).toBeGreaterThan(updateBox.y);

    const command = screen.getByTestId("startup-error-install-command");
    await expect(command).toHaveText(IN_CONTAINER);
    expect(await command.evaluate((el) => el.scrollWidth <= el.clientWidth)).toBe(true);
    // More than one line: it wraps rather than clipping.
    const lineHeight = await command.evaluate((el) => parseFloat(getComputedStyle(el).lineHeight));
    expect((await command.boundingBox())!.height).toBeGreaterThan(lineHeight * 1.5);
    await expectCopyIsAButton(screen);
    const overflow = await page.evaluate(
      () => document.documentElement.scrollWidth - document.documentElement.clientWidth,
    );
    expect(overflow).toBeLessThanOrEqual(0);
  });

  test("Recreate container fits beside the others and opens its confirmation on screen", async ({ page }) => {
    const { screen } = await openRefusedSession(page);
    const recreate = screen.getByRole("button", { name: "Recreate container" });
    await expect(recreate).toBeInViewport({ ratio: 1 });
    await recreate.click();
    const dialog = page.getByRole("dialog", { name: "Recreate container" });
    await expect(dialog).toContainText("story-adapter-out-of-date");
    await expect(page.getByTestId("recreate-container-confirm")).toBeInViewport({ ratio: 1 });
    const overflow = await page.evaluate(
      () => document.documentElement.scrollWidth - document.documentElement.clientWidth,
    );
    expect(overflow).toBeLessThanOrEqual(0);
    await dialog.getByRole("button", { name: "Cancel" }).click();
    await expect(dialog).toBeHidden();
  });
});
