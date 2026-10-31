import { test, expect } from "./helpers/mockedTest";
import { mockTerminalApis } from "./helpers/terminal-mocks";

const mk = (n: number, title: string, reason: string, project: string) => ({
  id: `i${n}`,
  provider_id: "github",
  title,
  reason,
  source_url: `https://example.test/${n}`,
  source_ref: `example/${n}`,
  preview: "",
  project,
  criterion: "assigned",
  draft: { title, prompt: "Do it." },
  dismissed: false,
});
const items = [
  mk(1, "#4562 Suggested sessions RFC: draft the homepage list", "Review requested from you", "agent-of-empires"),
  mk(2, "THY-112 Export timeline as PDF", "Assigned to you, due Friday", "Thymeline"),
  mk(3, "Add tests for the gift budget calculator", "No tests cover budget.ts", "GiftPlanner"),
  mk(4, "Fourth item stays out of the homepage list", "Assigned to you", "aoe"),
];
const provider = { id: "github", name: "GitHub", status: "ok", sources: [] };
const prefs = {
  homepage: true,
  sidebar: false,
  refresh_minutes: 15,
  start_without_review: false,
  provider_order: ["github"],
};
const CURSOR = { "aoe-runtime-epoch": "mock-runtime", "aoe-runtime-revision": "1" };

async function mockHome(page: import("@playwright/test").Page) {
  await mockTerminalApis(page);
  await page.route("**/api/sessions", (route) =>
    route.fulfill({ headers: CURSOR, json: { sessions: [], workspace_ordering: [] } }),
  );
  await page.route("**/api/suggestions", (route) => route.fulfill({ json: { providers: [provider], items, prefs } }));
}

test("homepage shows 3 suggestions and View all opens the full view", async ({ page }) => {
  await mockHome(page);
  await page.goto("/");
  await expect(page.getByRole("button", { name: "Start" })).toHaveCount(3);
  await expect(page.getByText("GitHub · Review requested from you · agent-of-empires")).toBeVisible();
  await page.getByRole("link", { name: "View all (4)" }).click();
  await expect(page).toHaveURL(/\/suggestions$/);
});

test("narrow viewport shows 2 suggestions", async ({ page }) => {
  await page.setViewportSize({ width: 375, height: 812 });
  await mockHome(page);
  await page.goto("/");
  await expect(page.getByRole("button", { name: "Start" })).toHaveCount(2);
});
