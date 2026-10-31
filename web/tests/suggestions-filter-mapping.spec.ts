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

  const chips = page.getByRole("group", { name: "Filter by source" });
  await chips.getByRole("button", { name: "acme/ 2" }).click();
  await expect(titles).toHaveText(["Web bug", "Api bug"]);
  await chips.getByRole("button", { name: "web 1" }).click();
  await chips.getByRole("button", { name: "acme/ 2" }).click();
  await expect(titles).toHaveText(["Web bug"]);

  await page.getByRole("button", { name: "Clear filters" }).click();
  await page.getByLabel("Search suggestions").fill("TOOL#3");
  await expect(titles).toHaveText(["Tool chore"]);
});
