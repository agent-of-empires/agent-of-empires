// Sidebar chrome: compact mode, empty state, project headers, new-session tooltips, and row chips and actions.

import type { Page } from "@playwright/test";
import { test, expect } from "./helpers/mockedTest";
import { installSidebarMocks, threeSessionsInOneRepo, type MockSessionInput } from "./helpers/sidebarMocks";

async function openSidebar(page: Page, sessions: MockSessionInput[]) {
  await installSidebarMocks(page, { sessions });
  await page.setViewportSize({ width: 1280, height: 720 });
  await page.goto("/");
}

const HEADER = "[data-testid='sidebar-group-header']";
const COUNT = "[data-testid='sidebar-group-session-count']";
const ROW = "[data-testid='sidebar-session-row']";

test.describe("compact mode (#2288)", () => {
  const SESSIONS = [
    { id: "s-a", title: "alpha-session", project_path: "/tmp/repo-alpha", branch: "feat/a" },
    { id: "s-b", title: "beta-session", project_path: "/tmp/repo-alpha", branch: "feat/b" },
  ];

  test("compact toggle slims the sidebar, hides extras and an open filter, stays tappable, and persists", async ({
    page,
  }) => {
    await openSidebar(page, SESSIONS);
    const panel = page.locator('[data-tour="sidebar"]');
    const width = async () => (await panel.boundingBox())!.width;
    await expect(panel).toBeVisible();
    expect(await width()).toBeGreaterThan(200);
    await expect(page.locator(COUNT).first()).toBeVisible();
    await expect(page.getByRole("button", { name: "New session in repo-alpha" })).toBeVisible();

    await page.getByRole("button", { name: "Compact sidebar" }).click();
    await expect.poll(width).toBeLessThan(120);
    await expect(page.locator(COUNT)).toHaveCount(0);
    await expect(page.getByRole("button", { name: "New session in repo-alpha" })).toHaveCount(0);
    await expect(page.getByRole("button", { name: "Expand sidebar" })).toBeVisible();
    await page.getByText("alpha-session").click();
    // Wide uppercase footer labels used to spill past the rail.
    await expect.poll(() => panel.evaluate((el) => el.scrollWidth - el.clientWidth)).toBeLessThanOrEqual(1);
    await expect(page.getByTestId("sidebar-projects-toggle")).toBeVisible();
    await expect(page.getByTestId("sidebar-projects-add")).toHaveCount(0);
    // The wordmark stands down; the logo still links home.
    await expect(page.getByRole("button", { name: "Go to dashboard" })).toBeVisible();
    await expect(page.getByText("aoe", { exact: true })).toBeHidden();

    await page.reload();
    await expect(page.getByRole("button", { name: "Expand sidebar" })).toBeVisible();
    await expect.poll(width).toBeLessThan(120);
    await page.getByRole("button", { name: "Expand sidebar" }).click();
    await expect.poll(width).toBeGreaterThan(200);
    await expect(page.locator(COUNT).first()).toBeVisible();

    // Entering compact hides an open filter and stops its query narrowing the list.
    await page.getByRole("button", { name: "Filter sessions" }).click();
    const filterInput = page.getByTestId("sidebar-filter-input");
    // "beta" matches a title only; "alpha" would also match the project name.
    await filterInput.fill("beta");
    await expect(page.getByText("alpha-session")).toHaveCount(0);
    await expect(page.getByText("beta-session")).toBeVisible();
    await page.getByRole("button", { name: "Compact sidebar" }).click();
    await expect(filterInput).toHaveCount(0);
    await expect(page.getByRole("button", { name: "Filter sessions" })).toHaveCount(0);
    await expect(page.getByText("alpha-session")).toBeVisible();
    await page.getByRole("button", { name: "Expand sidebar" }).click();
    await expect(filterInput).toHaveValue("beta");
    await expect(page.getByText("alpha-session")).toHaveCount(0);
  });
});

test("empty sidebar shows a hint that opens the wizard, and hides it once a session exists", async ({ page }) => {
  // #1835
  await openSidebar(page, []);
  const empty = page.getByTestId("sidebar-empty-state");
  await expect(empty).toContainText("No sessions yet");
  await expect(page.locator(ROW)).toHaveCount(0);
  await empty.getByRole("button", { name: "New session" }).click();
  await expect(page.getByRole("heading", { name: "New session" })).toBeVisible();

  await page.unrouteAll({ behavior: "ignoreErrors" });
  await openSidebar(page, [{ id: "s-a", title: "alpha", project_path: "/tmp/repo", branch: "feature/a" }]);
  await expect(page.locator(ROW)).toHaveCount(1);
  await expect(page.getByTestId("sidebar-empty-state")).toHaveCount(0);
});

test("the two new-session buttons have distinct tooltips and labels", async ({ page }) => {
  // #2205. Tooltips portal on hover only (#2214).
  await openSidebar(page, [{ id: "s-a", title: "alpha-session", project_path: "/tmp/repo-alpha", branch: "feat/a" }]);
  for (const [name, tooltip] of [
    ["New project session", "New project session"],
    // The project name stays out of the tooltip text.
    ["New session in repo-alpha", "New session"],
  ]) {
    const button = page.getByRole("button", { name });
    await expect(button).toBeVisible();
    await button.hover();
    await expect(page.getByRole("tooltip")).toHaveText(tooltip!);
  }
});

// #2207: no grip; the icon and fold chevron swap on hover (group-hover opacity); the count always shows.
test.describe("project header row (#2207)", () => {
  test("icon swaps for the fold chevron on hover, with no grab bar; the session count survives collapse", async ({
    page,
  }) => {
    await openSidebar(page, threeSessionsInOneRepo());
    const icon = page.getByTestId("sidebar-group-icon");
    const chevron = page.getByTestId("sidebar-group-fold-chevron");
    await expect(page.locator(HEADER)).toHaveCount(1);
    await expect(page.getByTestId("sidebar-group-drag-handle")).toHaveCount(0);
    await expect(icon).toHaveCSS("opacity", "1");
    await expect(chevron).toHaveCSS("opacity", "0");
    await page.locator(HEADER).hover();
    await expect(icon).toHaveCSS("opacity", "0");
    await expect(chevron).toHaveCSS("opacity", "1");

    await expect(page.locator(ROW)).toHaveCount(3);
    await expect(page.locator(COUNT)).toHaveText("(3)");
    await page.locator(COUNT).click();
    await expect(page.locator(ROW)).toHaveCount(0);
    await expect(page.locator(COUNT)).toHaveText("(3)");
    await page.locator(COUNT).click();
    await expect(page.locator(ROW)).toHaveCount(3);
  });

  test("a drag on the header does not collapse it (trailing click suppressed)", async ({ page }) => {
    await openSidebar(page, threeSessionsInOneRepo());
    await expect(page.locator(ROW)).toHaveCount(3);
    const box = (await page.locator(HEADER).boundingBox())!;
    const [x, y] = [box.x + 60, box.y + box.height / 2];
    // Past the 8px activation threshold and back, releasing over the header.
    await page.mouse.move(x, y);
    await page.mouse.down();
    await page.mouse.move(x + 16, y, { steps: 6 });
    await page.mouse.move(x, y, { steps: 6 });
    await page.mouse.up();
    await expect(page.locator(ROW)).toHaveCount(3);
    await expect(page.locator(COUNT)).toHaveText("(3)");
  });
});

test.describe("row chips and naming actions", () => {
  const structured = (id: string, title: string, defaultName: boolean): MockSessionInput => ({
    id,
    title,
    project_path: "/tmp/repo",
    branch: null,
    fields: { view: "structured", smart_rename: defaultName ? "pending" : "inactive", default_name: defaultName },
  });

  async function openMenu(page: Page, title: string) {
    await page.locator(ROW).filter({ hasText: title }).first().click({ button: "right" });
    await expect(page.getByTestId("sidebar-context-menu")).toBeVisible();
  }

  // #2347 Auto-name now and #2808 Summarize are offered for any structured session, named or not.
  test("context menu auto-name and summarize POST their endpoints", async ({ page }) => {
    const posted: string[] = [];
    await page.route(/\/api\/sessions\/[^/]+\/(smart-rename|summarize)$/, (r) => {
      if (r.request().method() !== "POST") return r.fulfill({ status: 400 });
      posted.push(new URL(r.request().url()).pathname);
      return r.fulfill({ status: 202 });
    });
    await openSidebar(page, [structured("sess-1", "Fix login bug", false)]);
    for (const item of ["auto-name", "summarize"]) {
      await openMenu(page, "Fix login bug");
      await page.getByTestId(`sidebar-context-menu-${item}`).click();
    }
    await expect.poll(() => posted).toEqual(["/api/sessions/sess-1/smart-rename", "/api/sessions/sess-1/summarize"]);
  });
});
