// Sidebar chrome: resize handle and compact rail, empty state, project headers, new-session tooltips, and row chips and actions.

import type { Page } from "@playwright/test";
import { test, expect } from "./helpers/mockedTest";
import { installSidebarMocks, threeSessionsInOneRepo, type MockSessionInput } from "./helpers/sidebarMocks";
import { dragSidebarHandle, openMobileSidebar } from "./helpers/sidebar";
import { iPhone13 } from "./helpers/viewports";

async function openSidebar(page: Page, sessions: MockSessionInput[]) {
  await installSidebarMocks(page, { sessions });
  await page.setViewportSize({ width: 1280, height: 720 });
  await page.goto("/");
}

const HEADER = "[data-testid='sidebar-group-header']";
const COUNT = "[data-testid='sidebar-group-session-count']";
const ROW = "[data-testid='sidebar-session-row']";

test("archived and snoozed labels retain full opacity and italic styling", async ({ page }) => {
  await openSidebar(
    page,
    ["archived", "snoozed"].map((state) => ({
      id: state,
      title: `${state}-session`,
      project_path: "/tmp/repo",
      branch: null,
      fields: {
        status: "Idle",
        idle_entered_at: null,
        unread: false,
        archived_at: state === "archived" ? "2026-01-01T00:00:00Z" : null,
        snoozed_until: state === "snoozed" ? "2099-01-01T00:00:00Z" : null,
      },
    })),
  );
  await page.getByTestId("sidebar-sunk-toggle").click();
  for (const state of ["archived", "snoozed"]) {
    const label = page.locator(ROW).getByTitle(`${state}-session`, { exact: true });
    await expect(label).toBeVisible();
    await expect(label).toHaveCSS("font-style", "italic");
    const opacity = await label.evaluate((el) => {
      let opacity = 1;
      for (let node: Element | null = el; node; node = node.parentElement) {
        opacity *= Number(getComputedStyle(node).opacity);
      }
      return opacity;
    });
    expect(opacity).toBe(1);
  }
});

test.describe("resize handle and compact rail (#2288)", () => {
  const SESSIONS = [
    { id: "s-a", title: "alpha-session", project_path: "/tmp/repo-alpha", branch: "feat/a" },
    { id: "s-b", title: "beta-session", project_path: "/tmp/repo-alpha", branch: "feat/b" },
  ];
  const panelWidth = (page: Page) => async () =>
    (await page.locator('[data-tour="sidebar"]').boundingBox())?.width ?? 0;

  test("dragging below the minimum snaps to the rail, which hides extras and an open filter, stays tappable, and persists", async ({
    page,
  }) => {
    await openSidebar(page, SESSIONS);
    const panel = page.locator('[data-tour="sidebar"]');
    const width = panelWidth(page);
    await expect(panel).toBeVisible();
    await expect(page.getByTestId("sidebar-compact-toggle")).toHaveCount(0);
    await expect(page.locator(COUNT).first()).toBeVisible();
    await expect(page.getByRole("button", { name: "New session in repo-alpha" })).toBeVisible();

    // The drag follows the pointer down to the 200px minimum and holds there until the snap point.
    await dragSidebarHandle(page, 230);
    await expect.poll(width).toBe(230);
    await dragSidebarHandle(page, 160);
    await expect.poll(width).toBe(200);
    await expect(panel).not.toHaveAttribute("data-compact", "true");

    await dragSidebarHandle(page, 100);
    await expect(panel).toHaveAttribute("data-compact", "true");
    await expect.poll(width).toBeLessThan(120);
    await expect(page.locator(COUNT)).toHaveCount(0);
    await expect(page.getByRole("button", { name: "New session in repo-alpha" })).toHaveCount(0);
    await page.locator('[data-tour="sidebar"]').getByText("alpha-session").click();
    // Wide uppercase footer labels used to spill past the rail.
    await expect.poll(() => panel.evaluate((el) => el.scrollWidth - el.clientWidth)).toBeLessThanOrEqual(1);
    await expect(page.getByTestId("sidebar-projects-toggle")).toBeVisible();
    await expect(page.getByTestId("sidebar-projects-add")).toHaveCount(0);
    // The wordmark stands down; the logo still links home.
    await expect(page.getByRole("button", { name: "Go to dashboard" })).toBeVisible();
    await expect(page.getByText("aoe", { exact: true })).toBeHidden();

    await page.reload();
    await expect(panel).toHaveAttribute("data-compact", "true");
    await expect.poll(width).toBeLessThan(120);
    await dragSidebarHandle(page, 260);
    await expect(panel).not.toHaveAttribute("data-compact", "true");
    await expect.poll(width).toBe(260);
    await expect(page.locator(COUNT).first()).toBeVisible();

    // Entering compact hides an open filter and stops its query narrowing the list.
    await page.getByTestId("sidebar-view-toggle").click();
    await page.getByTestId("sidebar-filter-toggle").click();
    const filterInput = page.getByTestId("sidebar-filter-input");
    // "beta" matches a title only; "alpha" would also match the project name.
    await filterInput.fill("beta");
    await expect(page.locator('[data-tour="sidebar"]').getByText("alpha-session")).toHaveCount(0);
    await expect(page.locator('[data-tour="sidebar"]').getByText("beta-session")).toBeVisible();
    await dragSidebarHandle(page, 100);
    await expect(filterInput).toHaveCount(0);
    await expect(page.getByTestId("sidebar-view-toggle")).toHaveCount(0);
    await expect(page.locator('[data-tour="sidebar"]').getByText("alpha-session")).toBeVisible();
    await dragSidebarHandle(page, 260);
    await expect(filterInput).toHaveValue("beta");
    await expect(page.locator('[data-tour="sidebar"]').getByText("alpha-session")).toHaveCount(0);
  });

  test("dragging past the rail closes the sidebar, and reopening restores the sidebar the drag started from", async ({
    page,
  }) => {
    await openSidebar(page, SESSIONS);
    const panel = page.locator('[data-tour="sidebar"]');
    const width = panelWidth(page);
    const toggle = page.getByRole("button", { name: "Toggle sidebar" });
    await dragSidebarHandle(page, 300);
    await expect.poll(width).toBe(300);

    // Through the rail and out again in one gesture: the sidebar comes back with the pointer.
    await dragSidebarHandle(page, 100, 10, 250);
    await expect(panel).toBeVisible();
    await expect.poll(width).toBe(250);

    await dragSidebarHandle(page, 100, 10);
    await expect(panel).toBeHidden();
    await expect(page.getByTestId("sidebar-resize-handle")).toBeHidden();
    await toggle.click();
    await expect(panel).toBeVisible();
    await expect(panel).not.toHaveAttribute("data-compact", "true");
    await expect.poll(width).toBe(250);
    expect(await page.evaluate(() => localStorage.getItem("aoe-sidebar-width"))).toBe("250");

    // From the rail, closing keeps the rail for the reopen; the top bar toggle and its shortcut stay in step.
    await dragSidebarHandle(page, 100);
    await expect(panel).toHaveAttribute("data-compact", "true");
    await dragSidebarHandle(page, 10);
    await expect(panel).toBeHidden();
    await page.keyboard.press("ControlOrMeta+b");
    await expect(panel).toBeVisible();
    await expect(panel).toHaveAttribute("data-compact", "true");
    await toggle.click();
    await expect(panel).toBeHidden();
    await toggle.click();
    await expect(panel).toHaveAttribute("data-compact", "true");
  });

  test("the handle is a keyboard separator: arrows resize and snap to the rail, Enter restores, double-click resets", async ({
    page,
  }) => {
    await openSidebar(page, SESSIONS);
    const panel = page.locator('[data-tour="sidebar"]');
    const width = panelWidth(page);
    const handle = page.getByRole("separator", { name: "Resize sidebar" });
    await handle.focus();
    await expect(handle).toHaveAttribute("aria-valuenow", "280");

    await page.keyboard.press("ArrowRight");
    await expect(handle).toHaveAttribute("aria-valuenow", "296");
    await expect.poll(width).toBe(296);
    await page.keyboard.press("End");
    await expect(handle).toHaveAttribute("aria-valuenow", "480");
    await page.keyboard.press("Home");
    await expect(panel).toHaveAttribute("data-compact", "true");
    await expect(handle).toHaveAttribute("aria-valuenow", "88");
    // The rail is as narrow as the keyboard goes; the handle stays focusable.
    await page.keyboard.press("ArrowLeft");
    await expect(panel).toBeVisible();
    await expect(handle).toBeFocused();
    await page.keyboard.press("Enter");
    await expect(handle).toHaveAttribute("aria-valuenow", "480");

    await handle.dblclick();
    await expect(panel).not.toHaveAttribute("data-compact", "true");
    await expect.poll(width).toBe(280);
    expect(await page.evaluate(() => localStorage.getItem("aoe-sidebar-width"))).toBe("280");
  });

  test("a phone's session list ignores a compact rail and a width saved on a wider screen", async ({ page }) => {
    await installSidebarMocks(page, { sessions: SESSIONS });
    await page.addInitScript(() => {
      localStorage.setItem("aoe-web-settings", JSON.stringify({ sidebarCompact: true }));
      localStorage.setItem("aoe-sidebar-width", "480");
    });
    await page.setViewportSize({ width: 390, height: 844 });
    await page.goto("/");
    await openMobileSidebar(page);
    const panel = page.locator('[data-tour="sidebar"]');
    await expect(panel).not.toHaveAttribute("data-compact", "true");
    await expect(page.locator(COUNT).first()).toBeVisible();
    expect((await panel.boundingBox())!.width).toBe(390);

    await page.setViewportSize({ width: 1280, height: 720 });
    await expect(panel).toHaveAttribute("data-compact", "true");
  });

  test("a mouse released outside the window ends the drag at its last width", async ({ page }) => {
    await openSidebar(page, SESSIONS);
    const width = panelWidth(page);
    const box = (await page.getByTestId("sidebar-resize-handle").boundingBox())!;
    const y = box.y + box.height / 2;
    await page.mouse.move(box.x + box.width / 2, y);
    await page.mouse.down();
    await page.mouse.move(350, y, { steps: 4 });
    // The release lands outside the window, so the page only sees the next move, with no button held.
    await page.evaluate(() =>
      document.dispatchEvent(
        new PointerEvent("pointermove", {
          clientX: 420,
          buttons: 0,
          pointerId: 1,
          pointerType: "mouse",
          bubbles: true,
        }),
      ),
    );
    await expect.poll(width).toBe(350);
    expect(await page.evaluate(() => localStorage.getItem("aoe-sidebar-width"))).toBe("350");
    await page.mouse.up();
  });
});

test.describe("resize handle on a touch tablet (#2288)", () => {
  test.use({ hasTouch: true, viewport: { width: 1024, height: 768 } });

  test("a touch drag snaps the in-flow sidebar to the rail, and a second finger on the handle does not take it over", async ({
    page,
  }) => {
    await installSidebarMocks(page, { sessions: threeSessionsInOneRepo() });
    await page.goto("/");
    const panel = page.locator('[data-tour="sidebar"]');
    const box = (await page.getByTestId("sidebar-resize-handle").boundingBox())!;
    const y = box.y + box.height / 2;
    // Playwright's touchscreen only taps, so the drag goes through CDP.
    const cdp = await page.context().newCDPSession(page);
    const touch = (type: string, x: number, other?: number) =>
      cdp.send("Input.dispatchTouchEvent", {
        type,
        touchPoints:
          type === "touchEnd" ? [] : [{ x, y, id: 1 }, ...(other === undefined ? [] : [{ x: other, y: 400, id: 2 }])],
      });
    await touch("touchStart", box.x + box.width / 2);
    await touch("touchStart", box.x + box.width / 2, box.x + box.width / 2);
    await touch("touchMove", box.x + box.width / 2, 20);
    await expect(panel).toBeVisible();
    await expect(panel).not.toHaveAttribute("data-compact", "true");
    for (const x of [250, 200, 150, 100]) await touch("touchMove", x, 20);
    await touch("touchEnd", 100);
    await expect(panel).toHaveAttribute("data-compact", "true");
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

test("the two new-session buttons have distinct labels", async ({ page }) => {
  // #2205. The sidebar's New session row names itself; a group's button names its project, kept out of the
  // tooltip text. Tooltips portal on hover only (#2214).
  await openSidebar(page, [{ id: "s-a", title: "alpha-session", project_path: "/tmp/repo-alpha", branch: "feat/a" }]);
  await expect(page.getByTestId("sidebar-new-session")).toHaveAccessibleName("New session");
  const groupButton = page.getByRole("button", { name: "New session in repo-alpha" });
  await groupButton.hover();
  await expect(page.getByRole("tooltip")).toHaveText("New session");
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
      await page.getByTestId("sidebar-context-menu-more").click();
      await page.getByTestId(`sidebar-context-menu-${item}`).click();
    }
    await expect.poll(() => posted).toEqual(["/api/sessions/sess-1/smart-rename", "/api/sessions/sess-1/summarize"]);
  });
});

test.describe("session list on a notched phone", () => {
  test.use(iPhone13);

  test("clears the status bar and keeps its footer above the home indicator", async ({ page }) => {
    const cdp = await page.context().newCDPSession(page);
    await cdp.send("Emulation.setSafeAreaInsetsOverride", { insets: { top: 47, bottom: 34 } });
    await installSidebarMocks(page, { sessions: threeSessionsInOneRepo() });
    await page.goto("/");
    await openMobileSidebar(page);

    const menu = (await page.getByRole("button", { name: "More options" }).boundingBox())!;
    expect(menu.y).toBeGreaterThanOrEqual(47);
    const settings = (await page.getByRole("button", { name: "Settings" }).boundingBox())!;
    expect(settings.y + settings.height).toBeLessThanOrEqual(page.viewportSize()!.height - 34);
  });
});
