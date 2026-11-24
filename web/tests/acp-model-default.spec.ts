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

/** Serves the Session's default as the Daemon resolves it, under `claude`, and records the saves to any
 *  profile and Agent. `put` answers them. */
async function mockDefaults(
  page: Page,
  current: { model: string | null; pinned: boolean },
  put: () => { status: number; json: unknown } = () => ({ status: 200, json: {} }),
) {
  const saves: unknown[] = [];
  const saveUrls: string[] = [];
  const patches: string[] = [];
  await page.route(
    (url) => url.pathname === "/api/sessions/sess-1/acp-default-model",
    (r) => r.fulfill({ json: { profile: "default", agent_key: "claude", ...current } }),
  );
  await page.route(
    (url) => /^\/api\/profiles\/[^/]+\/acp-defaults\/[^/]+\/model$/.test(url.pathname),
    (r) => {
      if (r.request().method() !== "PUT") return r.fallback();
      saves.push(r.request().postDataJSON());
      saveUrls.push(new URL(r.request().url()).pathname);
      return r.fulfill(put());
    },
  );
  // A listener, not a route: `mockAcpSession` registers its own `/api/settings` route later, which would answer first.
  page.on("request", (req) => {
    if (req.method() === "PATCH" && new URL(req.url()).pathname === "/api/settings") patches.push(req.url());
  });
  return { saves, saveUrls, patches };
}

async function openSession(page: Page, current: string, about: Record<string, unknown> = {}, agent = "claude") {
  const mock = await mockAcpSession(page, {
    title: "model-default",
    initialEvents: [model(current)],
    sessionFields: { acp_agent: agent, tool: agent },
    about,
  });
  await openStructuredSession(page, mock);
  await page.getByTestId("config-option-model").click();
}

const checkbox = (page: Page) => page.getByRole("menuitemcheckbox", { name: LABEL });

test("checking it saves only this Agent's model, and the toast's link opens Agent defaults in the app", async ({
  page,
}) => {
  const { saves, saveUrls, patches } = await mockDefaults(page, { model: "claude-sonnet-4-6", pinned: false });
  await openSession(page, "claude-opus-4-7");
  await expect(checkbox(page)).toHaveAttribute("aria-checked", "false");
  await checkbox(page).click();
  await expect(page.getByRole("menu")).toHaveCount(0);
  await expect(
    page.getByText("Claude Opus 4.7 is now the default for new Claude Code Sessions in Structured view"),
  ).toBeVisible();
  // Only the model goes to the Daemon, which writes it into this Agent's entry alone.
  expect(saves).toEqual([{ model: "claude-opus-4-7" }]);
  expect(saveUrls).toEqual(["/api/profiles/default/acp-defaults/claude/model"]);
  expect(patches).toEqual([]);

  await page.evaluate(() => ((window as unknown as { sameDocument: boolean }).sameDocument = true));
  await page.getByRole("button", { name: "Agent defaults ›" }).click();
  await expect(page).toHaveURL(/\/settings\/structured-view\?profile=default&field=acp\.acp_defaults$/);
  expect(await page.evaluate(() => (window as unknown as { sameDocument?: boolean }).sameDocument)).toBe(true);
});

test("a Session detected as another Agent saves that Agent's default, the one its spawn reads", async ({ page }) => {
  const { saves, saveUrls } = await mockDefaults(page, { model: null, pinned: false });
  await openSession(page, "claude-opus-4-7", {}, "corp-claude");
  await checkbox(page).click();
  await expect(
    page.getByText("Claude Opus 4.7 is now the default for new Claude Code Sessions in Structured view"),
  ).toBeVisible();
  expect(saves).toEqual([{ model: "claude-opus-4-7" }]);
  expect(saveUrls).toEqual(["/api/profiles/default/acp-defaults/claude/model"]);
});

test("the default is checked and can't be unchecked here", async ({ page }) => {
  const { saves } = await mockDefaults(page, { model: "claude-opus-4-7", pinned: false });
  await openSession(page, "claude-opus-4-7");
  await expect(checkbox(page)).toHaveAttribute("aria-checked", "true");
  await expect(checkbox(page)).toHaveAttribute("aria-disabled", "true");
  // Playwright waits for an aria-disabled item to enable, so force the click a user can still make.
  await checkbox(page).click({ force: true });
  await expect(checkbox(page)).toBeVisible();
  expect(saves).toEqual([]);
});

test("a pinned model shows a link to Agent defaults instead", async ({ page }) => {
  await mockDefaults(page, { model: "claude-opus-4-7", pinned: true });
  await openSession(page, "claude-sonnet-4-6");
  await expect(page.getByRole("menuitem", { name: "Pinned in Agent defaults ›" })).toBeVisible();
  await expect(checkbox(page)).toHaveCount(0);
  await page.getByRole("menuitem", { name: "Pinned in Agent defaults ›" }).click();
  await expect(page).toHaveURL(/\/settings\/structured-view\?profile=default&field=acp\.acp_defaults$/);
});

test("a pin added after the picker opened refuses the save and says so", async ({ page }) => {
  const { saves, patches } = await mockDefaults(page, { model: null, pinned: false }, () => ({
    status: 409,
    json: { error: "model_pinned", message: "Claude Code's model is pinned in Agent defaults; change it there." },
  }));
  await openSession(page, "claude-opus-4-7");
  await checkbox(page).click();
  await expect(page.getByText("Claude Code's model is pinned in Agent defaults. Nothing was changed.")).toBeVisible();
  expect(saves).toEqual([{ model: "claude-opus-4-7" }]);
  expect(patches).toEqual([]);
  await expect(page.getByText(/is now the default for new/)).toHaveCount(0);
});

test("the keyboard reaches and checks it", async ({ page }) => {
  const { saves } = await mockDefaults(page, { model: null, pinned: false });
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
  await mockDefaults(page, { model: null, pinned: false });
  await openSession(page, "claude-opus-4-7", { cityhall_mode: true });
  await expect(page.getByRole("menuitem", { name: /Claude Sonnet 4.6/ })).toBeVisible();
  await expect(checkbox(page)).toHaveCount(0);
});

test.describe("phone", () => {
  test.use({ viewport: { width: 375, height: 812 }, hasTouch: true, isMobile: true });

  test("the menu and the toast fit without sideways scroll", async ({ page }) => {
    await mockDefaults(page, { model: null, pinned: false });
    await openSession(page, "claude-opus-4-7");
    await expect(checkbox(page)).toBeInViewport({ ratio: 1 });
    await checkbox(page).tap();
    await expect(page.getByRole("button", { name: "Agent defaults ›" })).toBeInViewport({ ratio: 1 });
    expect(await page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth)).toBeLessThanOrEqual(0);
  });
});
