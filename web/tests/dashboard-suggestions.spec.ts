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
  show_source_ref: true,
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
  await expect(page.getByRole("button", { name: "Start", exact: true })).toHaveCount(3);
  await expect(page.getByText("Review requested from you · agent-of-empires")).toBeVisible();
  await page.getByRole("link", { name: "View all (4)" }).click();
  await expect(page).toHaveURL(/\/suggestions$/);
});

test("narrow viewport shows 2 suggestions", async ({ page }) => {
  await page.setViewportSize({ width: 375, height: 812 });
  await mockHome(page);
  await page.goto("/");
  await expect(page.getByRole("button", { name: "Start", exact: true })).toHaveCount(2);
});

test("a long source reference keeps its number in view on a narrow row", async ({ page }) => {
  await page.setViewportSize({ width: 375, height: 812 });
  await mockTerminalApis(page);
  await page.route("**/api/sessions", (route) =>
    route.fulfill({ headers: CURSOR, json: { sessions: [], workspace_ordering: [] } }),
  );
  const long = { ...items[0], source_ref: "agent-of-empires-organization/agent-of-empires-repository#4681" };
  await page.route("**/api/suggestions", (route) =>
    route.fulfill({ json: { providers: [provider], items: [long], prefs } }),
  );
  await page.goto("/");
  const ref = page.getByTestId("suggestion-source-ref");
  const number = ref.getByText("#4681", { exact: true });
  const [refBox, numberBox] = [await ref.boundingBox(), await number.boundingBox()];
  expect(numberBox!.x + numberBox!.width).toBeLessThanOrEqual(refBox!.x + refBox!.width + 0.5);
  expect(numberBox!.width).toBeGreaterThan(0);
});

test("a failed start and the undo bar fit a 375px phone", async ({ page }) => {
  await page.setViewportSize({ width: 375, height: 812 });
  await mockHome(page);
  await page.route("**/api/suggestions/start", (route) =>
    route.fulfill({
      status: 422,
      json: { error: "start_failed", reason: "Docker isn't running", retryable: true },
    }),
  );
  await page.route("**/api/suggestions/dismiss", (route) => route.fulfill({ json: {} }));
  await page.goto("/");
  const fits = async () =>
    expect(await page.evaluate(() => document.documentElement.scrollWidth)).toBeLessThanOrEqual(375);

  await page.getByRole("button", { name: "Start", exact: true }).first().click();
  await expect(page.getByRole("button", { name: "Try again" })).toBeVisible();
  const title = page.getByText(items[0]!.title, { exact: true });
  // The two buttons sit under the text instead of squeezing the title.
  expect((await title.boundingBox())!.width).toBeGreaterThan(250);
  await fits();

  await page.getByRole("button", { name: "Dismiss" }).nth(1).click();
  const undo = page.getByRole("button", { name: "Undo" });
  await expect(undo).toBeVisible();
  const bar = undo.locator("..");
  const box = (await bar.boundingBox())!;
  expect(box.x + box.width).toBeLessThanOrEqual(375);
  expect((await bar.getByText(/^Dismissed/).boundingBox())!.width).toBeGreaterThan(100);
  await fits();
});

test("the sidebar's Start options menu stays in view and takes a pointer click on desktop", async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 800 });
  await mockHome(page);
  await page.route("**/api/suggestions", (route) =>
    route.fulfill({ json: { providers: [provider], items, prefs: { ...prefs, sidebar: true } } }),
  );
  let dismissed: unknown = null;
  await page.route("**/api/suggestions/dismiss", (route) => {
    dismissed = route.request().postDataJSON();
    return route.fulfill({ json: {} });
  });
  await page.goto("/");
  await page.getByTestId("sidebar-suggested-toggle").click();
  const section = page.getByTestId("sidebar-suggested-section");
  await section.getByRole("button", { name: "Start options" }).first().click();
  const menu = section.getByRole("menu", { name: "Start options" });
  const box = (await menu.boundingBox())!;
  expect(box.x).toBeGreaterThanOrEqual(0);
  expect(box.x + box.width).toBeLessThanOrEqual(1280);
  expect(box.y + box.height).toBeLessThanOrEqual(800);
  // Nothing clips or covers it: the item's centre hits the item itself.
  const item = menu.getByRole("menuitem", { name: "Dismiss" });
  const ib = (await item.boundingBox())!;
  expect(
    await item.evaluate(
      (el, [x, y]) => el.contains(document.elementFromPoint(x!, y!)),
      [ib.x + ib.width / 2, ib.y + ib.height / 2],
    ),
  ).toBe(true);
  await page.mouse.click(ib.x + ib.width / 2, ib.y + ib.height / 2);
  await expect.poll(() => dismissed).toEqual({ provider_id: "github", id: "i1" });
});
