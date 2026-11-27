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
import { dragRow, openMobileSidebar, readVisibleSessionTitles } from "./helpers/sidebar";

const WRAPPERS = "[aria-roledescription='Press and hold to reorder']";

async function openDesktop(page: Page, sessions: MockSessionInput[] = threeSessionsInOneRepo(), opts = {}) {
  const handle = await installSidebarMocks(page, { sessions, ...opts });
  await page.setViewportSize({ width: 1280, height: 720 });
  await page.goto("/");
  return handle;
}

type DragProbeWindow = Window & {
  __aoeDragClickProbe?: { released: () => boolean; restore: () => void };
};

/** Observe the sensor's capturing click listener, not a presumed teardown delay. */
async function observeSensorRelease(page: Page) {
  await page.evaluate(() => {
    const add = document.addEventListener;
    const remove = document.removeEventListener;
    const pending = new Set<EventListenerOrEventListenerObject>();
    let seen = false;
    document.addEventListener = ((...args: Parameters<Document["addEventListener"]>) => {
      const [type, listener, options] = args;
      if (type === "click" && typeof options === "object" && options?.capture) {
        seen = true;
        pending.add(listener);
      }
      add.apply(document, args);
    }) as Document["addEventListener"];
    document.removeEventListener = ((...args: Parameters<Document["removeEventListener"]>) => {
      const [type, listener, options] = args;
      if (type === "click" && (options === true || (typeof options === "object" && options?.capture))) {
        pending.delete(listener);
      }
      remove.apply(document, args);
    }) as Document["removeEventListener"];
    (window as DragProbeWindow).__aoeDragClickProbe = {
      released: () => seen && pending.size === 0,
      restore: () => {
        document.addEventListener = add;
        document.removeEventListener = remove;
      },
    };
  });
}

async function clickAfterSensorRelease(page: Page, x: number, y: number) {
  await page.waitForFunction(() => (window as DragProbeWindow).__aoeDragClickProbe?.released(), null, {
    timeout: 5_000,
  });
  await page.evaluate(() => {
    const browser = window as DragProbeWindow;
    browser.__aoeDragClickProbe?.restore();
    delete browser.__aoeDragClickProbe;
  });
  await page.mouse.click(x, y);
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
      await expect(page.locator("[data-testid='sidebar-view-toggle']")).toHaveAttribute("data-sort-mode", "manual");
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

  test("a click right after a drop goes through: the row that didn't move navigates", async ({ page }) => {
    await openDesktop(page);
    const wrappers = page.locator(WRAPPERS);
    await expect(wrappers).toHaveCount(3);
    // alpha keeps its place when gamma drops onto beta, so its box is valid straight after the drop.
    const alpha = (await wrappers.nth(0).boundingBox())!;
    await observeSensorRelease(page);
    await dragRow(page, wrappers.nth(2), wrappers.nth(1));
    await clickAfterSensorRelease(page, alpha.x + alpha.width - 4, alpha.y + alpha.height / 2);
    await expect(page).toHaveURL(/\/session\/s-a$/, { timeout: 5_000 });
  });

  test("Escape cancels a drag without reordering, and the next click goes through", async ({ page }) => {
    const handle = await openDesktop(page);
    const wrappers = page.locator(WRAPPERS);
    await expect(wrappers).toHaveCount(3);
    const alpha = (await wrappers.nth(0).boundingBox())!;
    await observeSensorRelease(page);
    await dragRow(page, wrappers.nth(2), wrappers.nth(1), { release: false });
    // The drag must really be live, or the "nothing happened" checks below prove nothing.
    await expect(wrappers.nth(2)).toHaveClass(/ring-2/);
    // dnd-kit's pointer sensor cancels on Escape, which must release the click guard too.
    await page.keyboard.press("Escape");
    await page.mouse.up();
    await expectNoOrderingPut(page, handle, 200);
    expect(await readVisibleSessionTitles(page)).toEqual(["alpha", "beta", "gamma"]);
    await clickAfterSensorRelease(page, alpha.x + alpha.width - 4, alpha.y + alpha.height / 2);
    await expect(page).toHaveURL(/\/session\/s-a$/, { timeout: 5_000 });
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
    // A drop past the repo's own rows targets none of them, so it is not a move to the nearest.
    const sessions: MockSessionInput[] = [
      { id: "a1", title: "alpha-a", project_path: "/tmp/repo-a", branch: "feat/1" },
      { id: "a2", title: "beta-a", project_path: "/tmp/repo-a", branch: "feat/2" },
      { id: "b1", title: "alpha-b", project_path: "/tmp/repo-b", branch: "feat/1" },
      { id: "b2", title: "beta-b", project_path: "/tmp/repo-b", branch: "feat/2" },
    ];
    const handle = await openDesktop(page, sessions, { ordering: sessions.map(workspaceId) });
    const wrappers = page.locator(WRAPPERS);
    await expect(wrappers).toHaveCount(4);
    await dragRow(page, wrappers.nth(0), wrappers.nth(2));
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
    await openMobileSidebar(page);
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

  test("a click right after a header drop toggles the header just dropped", async ({ page }) => {
    await openDesktop(page, twoRepos);
    await expect(draggable(page)).toHaveCount(2);
    const before = await groupNames(page);
    const source = (await draggable(page).nth(1).boundingBox())!;
    const target = (await page.locator("[data-testid='sidebar-group-header']").nth(0).boundingBox())!;
    await observeSensorRelease(page);
    await page.mouse.move(source.x + 40, source.y + source.height / 2);
    await page.mouse.down();
    await page.mouse.move(target.x + 40, target.y + target.height / 3, { steps: 12 });
    await page.mouse.up();
    // The dropped header now sits where the target was.
    const dropped = page
      .locator("[data-testid='sidebar-group-header']")
      .filter({ hasText: before[1]! })
      .locator("button[aria-expanded]");
    await expect(dropped).toHaveAttribute("aria-expanded", "true");
    const box = (await dropped.boundingBox())!;
    await clickAfterSensorRelease(page, box.x + box.width / 2, box.y + box.height / 2);
    await expect(dropped).toHaveAttribute("aria-expanded", "false");
  });

  test("the Scratch header stays put: it offers no drag", async ({ page }) => {
    const scratch = {
      id: "s-x",
      title: "scratch-session",
      project_path: "/tmp/scratch",
      branch: null,
      fields: { scratch: true },
    };
    await openDesktop(page, [...twoRepos, scratch]);
    await expect(page.getByText("Scratch", { exact: true })).toBeVisible();
    await expect(draggable(page)).toHaveCount(2);
    await expect.poll(() => groupNames(page)).toEqual(["repo-beta", "repo-alpha", "Scratch"]);
  });

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

// Group headers follow each profile's `groups.json` order under Custom; one request arranges the siblings.
test.describe("Groups headers", () => {
  const grouped: MockSessionInput[] = ["alpha", "beta", "gamma"].map((group) => ({
    id: `s-${group}`,
    title: `${group}-session`,
    project_path: "/tmp/repo",
    branch: `feat/${group}`,
    group,
  }));
  const groupOrder = { default: ["alpha", "beta", "gamma"] };
  const headers = (page: Page) => page.locator("[data-testid='sidebar-group-header']");
  const names = (page: Page) =>
    page.evaluate(() =>
      Array.from(document.querySelectorAll<HTMLElement>("[data-testid='sidebar-group-header']"))
        .map((h) => h.querySelector("span[title]")?.textContent?.trim() ?? "")
        .filter(Boolean),
    );
  const dragHeader = async (page: Page, from: number, to: number) => {
    const source = (await headers(page).nth(from).boundingBox())!;
    const target = (await headers(page).nth(to).boundingBox())!;
    await page.mouse.move(source.x + 40, source.y + source.height / 2);
    await page.mouse.down();
    await page.mouse.move(target.x + 40, target.y + target.height / 3, { steps: 12 });
    await page.mouse.up();
  };

  test("under Custom a header drag moves the Group in one request, and persists", async ({ page }) => {
    const handle = await openDesktop(page, grouped, { groupOrder, view: { group_by: "group", sort: "custom" } });
    await expect.poll(() => names(page), { timeout: 8_000 }).toEqual(["alpha", "beta", "gamma"]);
    await expect(headers(page).and(page.locator("[data-draggable='true']"))).toHaveCount(3);

    await dragHeader(page, 2, 0);
    await expect.poll(() => names(page), { timeout: 4_000 }).toEqual(["gamma", "alpha", "beta"]);
    const step = {
      target: "group",
      group: { profile: "default", path: "gamma" },
      direction: "up",
      siblings: ["alpha", "gamma", "beta"],
    };
    await expect.poll(() => handle.reorders, { timeout: 4_000 }).toEqual([step]);
    expect(handle.puts).toEqual([]);
    await page.reload();
    await expect.poll(() => names(page), { timeout: 8_000 }).toEqual(["gamma", "alpha", "beta"]);
  });

  test("under Repository a nested Group header moves among the Groups of its repository", async ({ page }) => {
    const handle = await openDesktop(page, grouped, { groupOrder });
    const nested = page.getByTestId("sidebar-nested-subgroup").locator("[data-testid='sidebar-group-header']");
    await expect(nested).toHaveCount(3);
    await expect.poll(() => names(page), { timeout: 8_000 }).toEqual(["repo", "alpha", "beta", "gamma"]);
    await dragHeader(page, 1, 3);
    await expect.poll(() => names(page), { timeout: 4_000 }).toEqual(["repo", "beta", "gamma", "alpha"]);
    const step = {
      target: "group",
      group: { profile: "default", path: "alpha" },
      direction: "down",
      siblings: ["beta", "alpha", "gamma"],
    };
    await expect.poll(() => handle.reorders, { timeout: 4_000 }).toEqual([step]);
  });

  test("a tall Group header dropped on its sibling moves there, whatever lies under the lifted block", async ({
    page,
  }) => {
    const tall: MockSessionInput[] = [
      ...[1, 2, 3, 4, 5].map((n) => ({
        id: `s-a${n}`,
        title: `alpha-${n}`,
        project_path: "/tmp/repo",
        branch: `feat/a${n}`,
        group: "alpha",
      })),
      { id: "s-b", title: "beta-session", project_path: "/tmp/repo", branch: "feat/b", group: "beta" },
      { id: "s-o", title: "other-session", project_path: "/tmp/other", branch: "feat/o", group: "other" },
    ];
    const handle = await openDesktop(page, tall, {
      groupOrder: { default: ["alpha", "beta", "other"] },
      repositoryOrder: ["/tmp/repo", "/tmp/other"],
    });
    const nested = page.getByTestId("sidebar-nested-subgroup").locator("[data-testid='sidebar-group-header']");
    await expect(nested).toHaveCount(3);
    const source = (await nested.nth(0).boundingBox())!;
    const target = (await nested.nth(1).boundingBox())!;
    await page.mouse.move(source.x + 40, source.y + source.height / 2);
    await page.mouse.down();
    await page.mouse.move(target.x + 40, target.y + target.height, { steps: 12 });
    await page.mouse.up();
    await expect
      .poll(() => handle.reorders, { timeout: 4_000 })
      .toEqual([
        {
          target: "group",
          group: { profile: "default", path: "alpha" },
          direction: "down",
          siblings: ["alpha", "beta", "other"],
        },
      ]);
  });

  test("Ungrouped leads the Groups under Groups, as selection ranges count it", async ({ page }) => {
    await openDesktop(
      page,
      [{ id: "s-u", title: "loose-session", project_path: "/tmp/repo", branch: "feat/u" }, ...grouped],
      {
        groupOrder,
        view: { group_by: "group", sort: "custom" },
      },
    );
    await expect.poll(() => names(page), { timeout: 8_000 }).toEqual(["Ungrouped", "alpha", "beta", "gamma"]);
  });

  test("under Newest first a header drag switches to Custom from the headers on screen, then moves it", async ({
    page,
  }) => {
    const handle = await openDesktop(page, grouped, { groupOrder, view: { group_by: "group", sort: "newest" } });
    // Each Group's session is newer than the one before it.
    await expect.poll(() => names(page), { timeout: 8_000 }).toEqual(["gamma", "beta", "alpha"]);
    await dragHeader(page, 2, 0);
    await expect.poll(() => handle.viewPatches, { timeout: 4_000 }).toEqual([{ sort: "custom" }]);
    await expect.poll(() => names(page)).toEqual(["alpha", "gamma", "beta"]);
    await expect(page.getByText("Switched to Custom")).toBeVisible();
    expect(handle.reorders.at(-1)).toMatchObject({ group: { path: "alpha" }, direction: "up" });
    await page.reload();
    await expect.poll(() => names(page), { timeout: 8_000 }).toEqual(["alpha", "gamma", "beta"]);
  });

  test("a CityHall client moves a Group header onto its sibling in one request", async ({ page }) => {
    const handle = await openDesktop(page, grouped, {
      groupOrder,
      cityhall: true,
      view: { group_by: "group", sort: "custom" },
    });
    await waitForResponseBody(page, "/api/about");
    await expect.poll(() => names(page), { timeout: 8_000 }).toEqual(["alpha", "beta", "gamma"]);
    await expect(page.locator(WRAPPERS)).toHaveCount(0);
    await dragHeader(page, 2, 1);
    await expect.poll(() => names(page), { timeout: 4_000 }).toEqual(["alpha", "gamma", "beta"]);
    await expect
      .poll(() => handle.reorders, { timeout: 4_000 })
      .toEqual([
        {
          target: "group",
          group: { profile: "default", path: "gamma" },
          direction: "up",
          siblings: ["alpha", "beta", "gamma"],
        },
      ]);
    expect(handle.puts).toEqual([]);
  });

  test("a read-only viewer gets no draggable Group header", async ({ page }) => {
    await openDesktop(page, grouped, { groupOrder, readOnly: true, view: { group_by: "group", sort: "custom" } });
    await waitForResponseBody(page, "/api/about");
    await expect.poll(() => names(page), { timeout: 8_000 }).toEqual(["alpha", "beta", "gamma"]);
    await expect(page.locator("[data-testid='sidebar-group-header'][data-draggable='true']")).toHaveCount(0);
  });
});
