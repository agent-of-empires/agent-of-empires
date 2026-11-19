import { test, expect } from "./helpers/mockedTest";
import { mockTerminalApis } from "./helpers/terminal-mocks";

const base = {
  provider_id: "github",
  reason: "Assigned to you",
  source_url: "https://example.test/issues/1",
  preview: "",
  project: null,
  criterion: "assigned",
  draft: { title: "", prompt: "" },
  dismissed: false,
};
const items = [
  { ...base, id: "1", title: "Web bug", source_ref: "acme/web#1", mapping_key: "acme/web" },
  { ...base, id: "2", title: "Api bug", source_ref: "acme/api#2", mapping_key: "acme/api" },
  { ...base, id: "3", title: "Tool chore", source_ref: "other/tool#3", mapping_key: "other/tool" },
];
const provider = {
  id: "github",
  name: "GitHub",
  status: "ok",
  enabled: true,
  sources: [{ criterion: "assigned", label: "Assigned issues", description: "", enabled: true, variables: [] }],
};
const prefs = {
  homepage: true,
  sidebar: false,
  refresh_minutes: 15,
  start_without_review: false,
  show_source_ref: true,
  provider_order: ["github"],
};
const CURSOR = { "aoe-runtime-epoch": "mock-runtime", "aoe-runtime-revision": "1" };

test.beforeEach(async ({ page }) => {
  await mockTerminalApis(page);
  await page.route("**/api/sessions", (route) =>
    route.fulfill({ headers: CURSOR, json: { sessions: [], workspace_ordering: [] } }),
  );
  await page.route("**/api/suggestions", (route) => route.fulfill({ json: { providers: [provider], items, prefs } }));
});

test("search and source chips narrow unmapped suggestions", async ({ page }) => {
  await page.goto("/suggestions");
  const titles = page.locator("main").getByText(/^(Web bug|Api bug|Tool chore)$/);
  await expect(titles).toHaveCount(3);

  await page.getByRole("button", { name: "All repos", exact: true }).click();
  const menu = (name: RegExp) => page.getByRole("button", { name });
  await menu(/^acme \(org\)/).click();
  await expect(titles).toHaveText(["Web bug", "Api bug"]);
  await menu(/^web 1/)
    .first()
    .click();
  await menu(/^acme \(org\)/).click();
  await expect(titles).toHaveText(["Web bug"]);

  await page.getByRole("button", { name: "Clear filters" }).click();
  await page.getByLabel("Search suggestions").fill("TOOL#3");
  await expect(titles).toHaveText(["Tool chore"]);
});

test("the Map projects wizard proposes Projects and saves the checked ones", async ({ page }) => {
  await page.route("**/api/suggestions/projects", (route) =>
    route.fulfill({
      json: {
        projects: [
          { name: "website", path: "/w/site", origin: "Acme/Web" },
          { name: "api", path: "/w/api", origin: null },
        ],
      },
    }),
  );
  const puts: unknown[] = [];
  await page.route("**/api/suggestions/mappings", (route) => {
    if (route.request().method() === "PUT") {
      puts.push(route.request().postDataJSON());
      return route.fulfill({ json: route.request().postDataJSON() });
    }
    return route.fulfill({ json: { mappings: [] } });
  });
  await page.goto("/suggestions");
  await page.getByRole("button", { name: "Link a Project" }).first().click();

  const wizard = page.getByRole("dialog", { name: "Map projects" });
  await expect(wizard.getByLabel("Map acme/web")).toBeChecked();
  await expect(wizard.getByLabel("Map acme/api")).not.toBeChecked();
  await expect(wizard.getByLabel("Map other/tool")).toBeDisabled();
  await wizard.getByLabel("Map acme/api").check();
  await wizard.getByRole("button", { name: "Save 2 mappings" }).click();

  await expect(wizard).toHaveCount(0);
  expect(puts).toEqual([
    { provider_id: "github", mapping_key: "acme/api", projects: ["/w/api"] },
    { provider_id: "github", mapping_key: "acme/web", projects: ["/w/site"] },
  ]);
});

test("a row is one line on desktop and two lines on a phone", async ({ page }) => {
  const lines = async () => {
    const row = page.locator("li", { hasText: "Web bug" });
    const title = await row.getByText("Web bug", { exact: true }).boundingBox();
    const meta = await row.getByText(/Assigned to you/).boundingBox();
    return { title: title!, meta: meta! };
  };
  await page.setViewportSize({ width: 1100, height: 800 });
  await page.goto("/suggestions");
  await expect(page.getByText("Web bug", { exact: true })).toBeVisible();
  const wide = await lines();
  expect(wide.meta.y).toBeLessThan(wide.title.y + wide.title.height);

  await page.setViewportSize({ width: 375, height: 800 });
  await expect(page.getByText("Web bug", { exact: true })).toBeVisible();
  const narrow = await lines();
  expect(narrow.meta.y).toBeGreaterThanOrEqual(narrow.title.y + narrow.title.height - 1);
});
