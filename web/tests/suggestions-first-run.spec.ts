import { test, expect } from "./helpers/mockedTest";
import { mockSettingsApis } from "./helpers/apiMocks";
import { mockTerminalApis } from "./helpers/terminal-mocks";

const featured = {
  slug: "agent-of-empires/plugin-github",
  html_url: "https://github.com/agent-of-empires/plugin-github",
  description: "Assigned issues and review requests.",
  stars: 1,
  badge: "featured",
  suggestions: true,
  install_command: "aoe plugin install gh:agent-of-empires/plugin-github",
  source_avatar_url: "",
};
const noProvider = {
  providers: [],
  items: [],
  prefs: { homepage: true, sidebar: false, refresh_minutes: 15, provider_order: [] },
};

async function mockFirstRun(page: import("@playwright/test").Page) {
  const uiStatePatches: Record<string, string | null>[] = [];
  await mockTerminalApis(page);
  await mockSettingsApis(page);
  await page.route("**/api/suggestions", (r) => r.fulfill({ json: noProvider }));
  await page.route("**/api/plugins/discover*", (r) => r.fulfill({ json: { results: [featured] } }));
  await page.route(
    (url) => url.pathname === "/api/plugins",
    (r) => r.fulfill({ json: { plugins: [], load_errors: [] } }),
  );
  await page.route("**/api/app-state/web-ui-state", (r) => {
    if (r.request().method() === "PATCH") {
      uiStatePatches.push(r.request().postDataJSON());
      return r.fulfill({ json: { ok: true } });
    }
    return r.fulfill({ json: {} });
  });
  return uiStatePatches;
}

test("hint sits under the cards and above the 'press n' line", async ({ page }) => {
  await mockFirstRun(page);
  await page.goto("/");
  const hint = page.getByTestId("suggestions-hint");
  await expect(hint).toBeVisible();
  await expect(hint).toContainText("from your GitHub issues and PRs?");

  const [cards, hintBox, pressN] = await Promise.all([
    page.locator("[data-tour=dashboard-new-session]").boundingBox(),
    hint.boundingBox(),
    page.getByText("to create a", { exact: false }).boundingBox(),
  ]);
  expect(cards && hintBox && pressN).toBeTruthy();
  expect(hintBox!.y).toBeGreaterThanOrEqual(cards!.y + cards!.height);
  expect(pressN!.y).toBeGreaterThanOrEqual(hintBox!.y + hintBox!.height);
});

test("More sources in the Marketplace opens it with the Works with Suggestions chip on", async ({ page }) => {
  await mockFirstRun(page);
  await page.goto("/");
  await page.getByTestId("suggestions-hint-open").click();
  await page.getByTestId("suggestions-setup-marketplace").click();

  await expect(page).toHaveURL(/\/settings\/plugins\?marketplace=suggestions$/);
  await expect(page.getByTestId("plugins-tab-marketplace")).toHaveAttribute("aria-selected", "true");
  await expect(page.getByTestId("plugins-filter-suggestions")).toHaveAttribute("aria-pressed", "true");
});

test("dismissing the hint syncs the key to the server", async ({ page }) => {
  const patches = await mockFirstRun(page);
  await page.goto("/");
  await page.getByTestId("suggestions-hint-dismiss").click();
  await expect(page.getByTestId("suggestions-hint")).toHaveCount(0);
  await expect.poll(() => patches.some((p) => p["aoe-suggestions-hint-dismissed"] === "1")).toBe(true);
});
