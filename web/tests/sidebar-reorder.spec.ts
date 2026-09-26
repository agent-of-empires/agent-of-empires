// Sidebar drag-to-reorder (#1169, #1419, #1644). dnd-kit activates on 8px of mouse movement, or a
// 150ms touch hold with under 8px of movement.

import { devices, type CDPSession, type Page } from "@playwright/test";
import { test, expect, observeFor, publishedRequests, waitForResponseBody } from "./helpers/mockedTest";
import {
  installSidebarMocks,
  threeSessionsInOneRepo,
  workspaceId,
  type MockSessionInput,
  type SidebarMockHandle,
} from "./helpers/sidebarMocks";
import { dragRow, readVisibleSessionTitles } from "./helpers/sidebar";

const WRAPPERS = "[aria-roledescription='Press and hold to reorder']";

async function openDesktop(page: Page, sessions: MockSessionInput[] = threeSessionsInOneRepo(), opts = {}) {
  const handle = await installSidebarMocks(page, { sessions, ...opts });
  await page.setViewportSize({ width: 1280, height: 720 });
  await page.goto("/");
  return handle;
}

async function expectNoOrderingPut(page: Page, handle: SidebarMockHandle, ms: number) {
  await observeFor(page, ms, async () => {
    expect(await publishedRequests(page, "/api/workspace-ordering", "PUT")).toEqual([]);
    expect(handle.puts).toEqual([]);
  });
  expect(handle.puts).toEqual([]);
}

test.describe("desktop rows", () => {
  test("applies the server-supplied ordering on first paint, verbatim", async ({ page }) => {
    const sessions: MockSessionInput[] = [
      {
        id: "s-old",
        title: "old-ws",
        project_path: "/tmp/repo",
        branch: "feature/old",
        created_at: "2025-01-01T00:00:00Z",
      },
      {
        id: "s-new",
        title: "new-ws",
        project_path: "/tmp/repo",
        branch: "feature/new",
        created_at: "2025-04-01T00:00:00Z",
      },
    ];
    // Both orders render as given, even the one contradicting created_at.
    for (const order of [
      ["old-ws", "new-ws"],
      ["new-ws", "old-ws"],
    ]) {
      await page.unrouteAll({ behavior: "ignoreErrors" });
      await openDesktop(page, sessions, {
        ordering: order.map((title) => workspaceId(sessions.find((s) => s.title === title)!)),
      });
      // #1418: drag needs manual sort, which must stay the default.
      await expect(page.locator("[data-testid='sidebar-sort-toggle']")).toHaveAttribute("data-sort-mode", "manual");
      await expect.poll(() => readVisibleSessionTitles(page), { timeout: 8_000 }).toEqual(order);
    }
  });

  test("press-and-hold drag lifts the row, PUTs the new order once, suppresses the trailing click, and persists", async ({
    page,
  }) => {
    const sessions = threeSessionsInOneRepo();
    const handle = await openDesktop(page, sessions, { persistPutOrdering: true });
    await expect.poll(() => readVisibleSessionTitles(page), { timeout: 8_000 }).toEqual(["alpha", "beta", "gamma"]);
    const wrappers = page.locator(WRAPPERS);
    await expect(wrappers).toHaveCount(3);
    const putWait = page.waitForResponse(
      (r) => r.url().endsWith("/api/workspace-ordering") && r.request().method() === "PUT" && r.status() < 400,
      { timeout: 8_000 },
    );

    await dragRow(page, wrappers.nth(2), wrappers.nth(0), { release: false });
    const sourceClass = (await wrappers.nth(2).getAttribute("class")) ?? "";
    expect(sourceClass).toContain("ring-2");
    expect(sourceClass).toContain("shadow-lg");
    await page.mouse.up();

    await putWait;
    expect(handle.puts.map((p) => p.order)).toEqual([[sessions[2]!, sessions[0]!, sessions[1]!].map(workspaceId)]);
    // Chromium dispatches a click on the drop target after mouseup; navigating there would defeat the drag.
    await observeFor(page, 400, async () => {
      expect(new URL(page.url()).pathname).toBe("/");
    });
    await expect.poll(() => readVisibleSessionTitles(page), { timeout: 4_000 }).toEqual(["gamma", "alpha", "beta"]);
    // The reloaded order comes from the served ordering, not a client sort.
    await page.reload();
    await expect.poll(() => readVisibleSessionTitles(page), { timeout: 8_000 }).toEqual(["gamma", "alpha", "beta"]);
  });

  test("a 4px movement does not start a drag; a stationary click navigates without reordering", async ({ page }) => {
    const handle = await openDesktop(page);
    const wrappers = page.locator(WRAPPERS);
    await expect(wrappers).toHaveCount(3);
    expect(await readVisibleSessionTitles(page)).toEqual(["alpha", "beta", "gamma"]);
    const moved = (await wrappers.nth(2).boundingBox())!;
    await page.mouse.move(moved.x + moved.width - 4, moved.y + moved.height / 2);
    await page.mouse.down();
    await page.mouse.move(moved.x + moved.width - 4, moved.y + moved.height / 2 + 4);
    await page.mouse.up();
    await expectNoOrderingPut(page, handle, 200);
    expect(await readVisibleSessionTitles(page)).toEqual(["alpha", "beta", "gamma"]);

    const box = (await wrappers.nth(1).boundingBox())!;
    await page.mouse.move(box.x + box.width - 4, box.y + box.height / 2);
    await page.mouse.down();
    await page.mouse.up();
    await expect(page).toHaveURL(/\/session\/s-b$/, { timeout: 5_000 });
    await expectNoOrderingPut(page, handle, 200);
  });

  test("dragging a row onto a different repo group is a no-op", async ({ page }) => {
    // Each repo has its own SortableContext, so a cross-group drop never reaches the reorder handler.
    const sessions: MockSessionInput[] = [
      { id: "a1", title: "alpha-a", project_path: "/tmp/repo-a", branch: "feat/1" },
      { id: "a2", title: "beta-a", project_path: "/tmp/repo-a", branch: "feat/2" },
      { id: "b1", title: "alpha-b", project_path: "/tmp/repo-b", branch: "feat/1" },
      { id: "b2", title: "beta-b", project_path: "/tmp/repo-b", branch: "feat/2" },
    ];
    const handle = await openDesktop(page, sessions, { ordering: sessions.map(workspaceId) });
    const wrappers = page.locator(WRAPPERS);
    await expect(wrappers).toHaveCount(4);
    await dragRow(page, wrappers.nth(1), wrappers.nth(2));
    await expectNoOrderingPut(page, handle, 300);
    expect(await readVisibleSessionTitles(page)).toEqual(["alpha-a", "beta-a", "alpha-b", "beta-b"]);
  });

  test("read-only viewer cannot drag sidebar rows", async ({ page }) => {
    const handle = await openDesktop(page, threeSessionsInOneRepo(), { readOnly: true });
    await waitForResponseBody(page, "/api/about");
    const rows = page.getByTestId("sidebar-session-row");
    await expect(rows).toHaveCount(3);
    await expect.poll(() => page.locator(WRAPPERS).count(), { timeout: 5_000 }).toBe(0);

    // Try anyway: re-enabled listeners would show a lift mid-gesture or a PUT.
    await dragRow(page, rows.nth(2), rows.nth(0), { release: false });
    expect(await page.locator(".ring-2.ring-brand-500").count()).toBe(0);
    await page.mouse.up();
    await expectNoOrderingPut(page, handle, 300);
  });
});

test.describe("touch rows", () => {
  const { defaultBrowserType: _, ...iPhone } = devices["iPhone 13"];
  test.use(iPhone);

  /** Open the mobile sidebar and wait for rows to slide in. Playwright's touchscreen only taps, so gestures use CDP. */
  async function openMobile(page: Page, sessions = threeSessionsInOneRepo()) {
    const handle = await installSidebarMocks(page, { sessions });
    await page.goto("/");
    await page.getByRole("button", { name: "Toggle sidebar" }).click();
    const rows = page.getByTestId("sidebar-session-row");
    await expect(rows).toHaveCount(3);
    await page.waitForFunction(
      () => {
        const r = document.querySelector('[data-testid="sidebar-session-row"]')?.getBoundingClientRect();
        return !!r && r.x >= 0 && r.width > 0;
      },
      null,
      { timeout: 5_000 },
    );
    return { handle, rows, cdp: await page.context().newCDPSession(page) };
  }
  const touch = (cdp: CDPSession, type: "touchStart" | "touchMove" | "touchEnd", points: { x: number; y: number }[]) =>
    cdp.send("Input.dispatchTouchEvent", { type, touchPoints: points.map((p) => ({ ...p, id: 1 })) });
  const center = async (row: import("@playwright/test").Locator) => {
    const box = (await row.boundingBox())!;
    return { x: box.x + box.width / 2, y: box.y + box.height / 2 };
  };

  test("a touch flick does not reorder rows; a tap navigates", async ({ page }) => {
    // Movement past tolerance before the hold delay cancels activation.
    const { handle, rows, cdp } = await openMobile(page);
    const { x, y } = await center(rows.nth(0));
    await touch(cdp, "touchStart", [{ x, y }]);
    for (const dy of [40, 120, 240]) await touch(cdp, "touchMove", [{ x, y: y + dy }]);
    await touch(cdp, "touchEnd", []);
    await expectNoOrderingPut(page, handle, 300);

    const tap = await center(rows.nth(1));
    await page.touchscreen.tap(tap.x, tap.y);
    await expect(page).toHaveURL(/\/session\/s-b$/, { timeout: 5_000 });
    await expectNoOrderingPut(page, handle, 200);
  });

  test("touch press-and-hold drag reorders the row and PUTs the new order", async ({ page }) => {
    const sessions = threeSessionsInOneRepo();
    const { handle, cdp } = await openMobile(page, sessions);
    const wrappers = page.locator(WRAPPERS);
    await expect(wrappers).toHaveCount(3);
    const from = await center(wrappers.nth(2));
    const to = await center(wrappers.nth(0));
    await touch(cdp, "touchStart", [from]);
    await page.waitForTimeout(220);
    // Walk the touch frame by frame so collision detection resolves the target row.
    for (let i = 1; i <= 8; i++) {
      await touch(cdp, "touchMove", [{ x: from.x + ((to.x - from.x) * i) / 8, y: from.y + ((to.y - from.y) * i) / 8 }]);
      await page.waitForTimeout(20);
    }
    await touch(cdp, "touchEnd", []);
    await expect.poll(() => handle.puts.length, { timeout: 4_000 }).toBe(1);
    expect(handle.puts[0]?.order?.[0]).toBe(workspaceId(sessions[2]!));
  });
});

// Group order is client-only (localStorage); the whole real header is the drag activator (#2207).
test.describe("group headers", () => {
  const twoRepos: MockSessionInput[] = [
    { id: "s-a", title: "alpha-session", project_path: "/tmp/repo-alpha", branch: "feat/a" },
    { id: "s-b", title: "beta-session", project_path: "/tmp/repo-beta", branch: "feat/b" },
  ];
  const groupNames = (page: Page) =>
    page.evaluate(() =>
      Array.from(document.querySelectorAll<HTMLElement>("[data-testid='sidebar-group-header']"))
        .map((h) => h.querySelector("span[title]")?.textContent?.trim() ?? "")
        .filter(Boolean),
    );
  const draggable = (page: Page) => page.locator("[data-testid='sidebar-group-header'][data-draggable='true']");

  test("drag a group header to reorder, order persists across reload", async ({ page }) => {
    await openDesktop(page, twoRepos);
    await expect(draggable(page)).toHaveCount(2);
    const before = await groupNames(page);
    expect(before).toHaveLength(2);

    // Grab near the name, clear of the new-session button.
    const source = (await draggable(page).nth(1).boundingBox())!;
    const target = (await page.locator("[data-testid='sidebar-group-header']").nth(0).boundingBox())!;
    await page.mouse.move(source.x + 40, source.y + source.height / 2);
    await page.mouse.down();
    await page.mouse.move(target.x + 40, target.y + target.height / 3, { steps: 12 });
    await page.mouse.up();

    const expected = [before[1], before[0]];
    await expect.poll(() => groupNames(page), { timeout: 4_000 }).toEqual(expected);
    await page.reload();
    await expect(draggable(page)).toHaveCount(2);
    await expect.poll(() => groupNames(page), { timeout: 4_000 }).toEqual(expected);
  });
});
