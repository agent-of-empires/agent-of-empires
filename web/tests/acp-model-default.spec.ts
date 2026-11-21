// #5909: making a Structured view Session's picked model its Agent's default, from the model picker.

import type { Page } from "@playwright/test";
import { test, expect } from "./helpers/mockedTest";
import { mockAcpSession, openStructuredSession, configOptionsUpdated } from "./helpers/acpMock";

const LABEL = "Default for new Claude Code Sessions in Structured view";
const model = (current: string) =>
  configOptionsUpdated([
    {
      id: "model",
      name: "Model",
      category: "model",
      current_value: current,
      options: [
        { value: "claude-opus-4-7", name: "Claude Opus 4.7" },
        { value: "claude-sonnet-4-6", name: "Claude Sonnet 4.6" },
      ],
    },
  ]);

/** Serves the profile's resolved and own `acp_defaults`, and records settings saves. */
async function mockDefaults(page: Page, resolved: Record<string, unknown>, own: Record<string, unknown>) {
  const saves: Array<{ url: string; body: unknown }> = [];
  await page.route(
    (url) => url.pathname === "/api/settings",
    (r) => {
      if (r.request().method() !== "PATCH") return r.fulfill({ json: { acp: { acp_defaults: resolved } } });
      saves.push({ url: r.request().url(), body: r.request().postDataJSON() });
      return r.fulfill({ json: {} });
    },
  );
  await page.route(
    (url) => url.pathname === "/api/profiles/default/settings",
    (r) => r.fulfill({ json: { acp: { acp_defaults: own } } }),
  );
  return saves;
}

async function openSession(page: Page, current: string, about: Record<string, unknown> = {}) {
  const mock = await mockAcpSession(page, {
    title: "model-default",
    initialEvents: [model(current)],
    sessionFields: { acp_agent: "claude" },
    about,
  });
  await openStructuredSession(page, mock);
  await page.getByTestId("config-option-model").click();
}

const checkbox = (page: Page) => page.getByRole("menuitemcheckbox", { name: LABEL });

test("checking it saves only this Agent's model, and the toast's link opens Agent defaults in the app", async ({
  page,
}) => {
  const saves = await mockDefaults(
    page,
    { claude: { model: "claude-sonnet-4-6", effort: "high" }, codex: { model: "gpt-5" } },
    { claude: { mode: "plan" } },
  );
  await openSession(page, "claude-opus-4-7");
  await expect(checkbox(page)).toHaveAttribute("aria-checked", "false");
  await checkbox(page).click();
  await expect(page.getByRole("menu")).toHaveCount(0);
  await expect(
    page.getByText("Claude Opus 4.7 is now the default for new Claude Code Sessions in Structured view"),
  ).toBeVisible();
  // The profile's own entries go back with one field changed; global's effort and codex stay inherited.
  expect(saves).toEqual([
    {
      url: expect.stringContaining("/api/settings?profile=default"),
      body: { acp: { acp_defaults: { claude: { mode: "plan", model: "claude-opus-4-7" } } } },
    },
  ]);

  await page.evaluate(() => ((window as unknown as { sameDocument: boolean }).sameDocument = true));
  await page.getByRole("button", { name: "Agent defaults ›" }).click();
  await expect(page).toHaveURL(/\/settings\/structured-view\?profile=default&field=acp\.acp_defaults$/);
  expect(await page.evaluate(() => (window as unknown as { sameDocument?: boolean }).sameDocument)).toBe(true);
});

test("the default is checked and can't be unchecked here", async ({ page }) => {
  const saves = await mockDefaults(page, { claude: { model: "claude-opus-4-7" } }, {});
  await openSession(page, "claude-opus-4-7");
  await expect(checkbox(page)).toHaveAttribute("aria-checked", "true");
  await expect(checkbox(page)).toHaveAttribute("aria-disabled", "true");
  // Playwright waits for an aria-disabled item to enable, so force the click a user can still make.
  await checkbox(page).click({ force: true });
  await expect(checkbox(page)).toBeVisible();
  expect(saves).toEqual([]);
});

test("a pinned model shows a link to Agent defaults instead", async ({ page }) => {
  await mockDefaults(page, { claude: { model: "claude-opus-4-7", pin_model: true } }, {});
  await openSession(page, "claude-sonnet-4-6");
  await expect(page.getByRole("menuitem", { name: "Pinned in Agent defaults ›" })).toBeVisible();
  await expect(checkbox(page)).toHaveCount(0);
  await page.getByRole("menuitem", { name: "Pinned in Agent defaults ›" }).click();
  await expect(page).toHaveURL(/\/settings\/structured-view\?profile=default&field=acp\.acp_defaults$/);
});

test("the keyboard reaches and checks it", async ({ page }) => {
  const saves = await mockDefaults(page, {}, {});
  await openSession(page, "claude-opus-4-7");
  // Loaded, not the default: neither checked nor disabled.
  await expect(checkbox(page)).not.toHaveAttribute("aria-disabled", "true");
  await page.keyboard.press("ArrowUp");
  await expect(checkbox(page)).toBeFocused();
  await page.keyboard.press("Enter");
  await expect.poll(() => saves.length).toBe(1);
  await expect(page.getByTestId("config-option-model")).toBeFocused();
});

test("CityHall dashboards don't offer it", async ({ page }) => {
  await mockDefaults(page, {}, {});
  await openSession(page, "claude-opus-4-7", { cityhall_mode: true });
  await expect(page.getByRole("menuitem", { name: /Claude Sonnet 4.6/ })).toBeVisible();
  await expect(checkbox(page)).toHaveCount(0);
});

test.describe("phone", () => {
  test.use({ viewport: { width: 375, height: 812 }, hasTouch: true, isMobile: true });

  test("the menu and the toast fit without sideways scroll", async ({ page }) => {
    await mockDefaults(page, {}, {});
    await openSession(page, "claude-opus-4-7");
    await expect(checkbox(page)).toBeInViewport({ ratio: 1 });
    await checkbox(page).tap();
    await expect(page.getByRole("button", { name: "Agent defaults ›" })).toBeInViewport({ ratio: 1 });
    expect(await page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth)).toBeLessThanOrEqual(0);
  });
});
