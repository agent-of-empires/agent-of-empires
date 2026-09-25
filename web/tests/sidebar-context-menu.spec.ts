// Sidebar row and group context menus: viewport clamping, multi-select bulk
// actions, the switch-view confirm gate, the context-resume badge, and the
// Android long-press guard.

import type { Locator, Page, Route } from "@playwright/test";
import { test, expect } from "./helpers/mockedTest";
import { iPhone13 } from "./helpers/viewports";
import { installSidebarMocks, threeSessionsInOneRepo, type MockSessionInput } from "./helpers/sidebarMocks";
import { openMobileSidebar } from "./helpers/sidebar";

const ROW = "[data-testid='sidebar-session-row']";
const MENU = "[data-testid='sidebar-context-menu']";
const GROUP_MENU = "[data-testid='sidebar-group-context-menu']";

const rows = (page: Page) => page.locator(ROW);
const selectedRows = (page: Page) => page.locator(`${ROW}[data-selected]`);
const menu = (page: Page) => page.locator(MENU);

/** Three sessions, one per repo, so each gets its own group header. */
const THREE: MockSessionInput[] = ["Mongols", "Goths", "Persians"].map((title, i) => ({
  id: `s-${i + 1}`,
  title,
  project_path: `/tmp/repo-${"abc"[i]}`,
  group: `/tmp/repo-${"abc"[i]}`,
  branch: null,
}));

async function openSidebar(page: Page, sessions: MockSessionInput[]) {
  await installSidebarMocks(page, { sessions });
  await page.goto("/");
  await expect(page.locator("header")).toBeVisible();
}

// #1601: menus opened near the bottom or right edge must clamp inside the
// viewport so every item stays reachable.
test.describe("Sidebar context-menu viewport clamp (#1601)", () => {
  async function expectClamped(page: Page, selector: string) {
    // Web fonts and icons can grow the menu after first paint and the component
    // reclamps via ResizeObserver, so let fonts settle before sampling.
    await page.evaluate(() => document.fonts?.ready);
    const viewport = page.viewportSize()!;
    await expect
      .poll(
        async () => {
          const box = await page.locator(selector).boundingBox();
          if (!box) return null;
          return (
            box.x >= 0 && box.y >= 0 && box.x + box.width <= viewport.width && box.y + box.height <= viewport.height
          );
        },
        { timeout: 5_000 },
      )
      .toBe(true);
  }

  // #2870: the cap must use the dynamic viewport height. `100vh` overshoots the
  // visible viewport on iOS Safari, so a menu taller than the visible area never
  // exceeds its own max-height and `overflow-y-auto` never engages, leaving
  // Delete behind the toolbar with no way to scroll to it.
  async function expectDvhCap(target: Locator) {
    expect(await target.evaluate((el) => el.style.maxHeight)).toContain("dvh");
    await expect(target).toHaveClass(/overflow-y-auto/);
  }

  test("right-click on the bottom session row and repo group header keeps each menu inside the viewport", async ({
    page,
  }) => {
    await installSidebarMocks(page, { sessions: THREE });
    await page.setViewportSize({ width: 900, height: 360 });
    await page.goto("/");
    await expect(page.locator("header")).toBeVisible();

    for (const [anchor, selector] of [
      [ROW, MENU],
      ["[data-testid='sidebar-group-header']", GROUP_MENU],
    ] as const) {
      const anchors = page.locator(anchor);
      await expect(anchors).toHaveCount(3);
      await anchors.last().scrollIntoViewIfNeeded();
      await anchors.last().click({ button: "right" });

      const target = page.locator(selector);
      await expect(target).toBeVisible();
      await expectClamped(page, selector);
      await expectDvhCap(target);
      await page.mouse.click(5, 5);
      await expect(target).toBeHidden();
    }
  });
});

// #1724, #2312: Cmd/Ctrl+click toggles a row into the selection without
// navigating, Shift+click extends the range, and bulk triage runs from the
// right-click menu (the BulkActionBar popup was removed in #2312).
test.describe("Sidebar multi-select (#1724, #2312)", () => {
  test("right-click a selected row bulk-archives the whole selection", async ({ page }) => {
    const archived: Array<{ id: string; body: unknown }> = [];
    await page.route("**/api/sessions/*/archive", (r) => {
      const id =
        r
          .request()
          .url()
          .match(/\/api\/sessions\/([^/]+)\/archive$/)?.[1] ?? "?";
      archived.push({ id, body: r.request().postDataJSON() });
      return r.fulfill({ json: { id, archived_at: "now" } });
    });
    await openSidebar(page, THREE);
    await expect(rows(page)).toHaveCount(3);

    // Anchor with Cmd+click (which does not navigate), then Shift+click the last.
    await rows(page)
      .nth(0)
      .click({ modifiers: ["ControlOrMeta"] });
    await rows(page)
      .nth(2)
      .click({ modifiers: ["Shift"] });
    await expect(selectedRows(page)).toHaveCount(3);

    await rows(page).nth(1).click({ button: "right" });
    await expect(menu(page)).toContainText("3 selected");
    const archiveItem = menu(page).locator("[data-testid='sidebar-context-menu-bulk-archive']");
    await expect(archiveItem).toContainText("Archive 3");
    await archiveItem.click();

    await expect.poll(() => archived.length).toBe(3);
    expect(archived.map((a) => a.id).sort()).toEqual(["s-1", "s-2", "s-3"]);
    for (const a of archived) expect(a.body).toEqual({ archived: true, kill_pane: true });
    await expect(selectedRows(page)).toHaveCount(0);
  });

  test("right-click on an unselected row resets the selection; Shift+click ranges from a navigated row (#2312)", async ({
    page,
  }) => {
    await openSidebar(page, THREE);
    await rows(page)
      .nth(0)
      .click({ modifiers: ["ControlOrMeta"] });
    await rows(page)
      .nth(1)
      .click({ modifiers: ["ControlOrMeta"] });
    await expect(selectedRows(page)).toHaveCount(2);

    await rows(page).nth(2).click({ button: "right" });
    await expect(selectedRows(page)).toHaveCount(1);
    await expect(menu(page)).toBeVisible();
    await expect(menu(page)).not.toContainText("selected");
    await expect(menu(page).locator("[data-testid='sidebar-context-menu-bulk-archive']")).toHaveCount(0);
    await page.keyboard.press("Escape");
    await page.mouse.click(5, 5);
    await expect(menu(page)).toBeHidden();

    // A plain click navigates and leaves the row as the anchor; no intervening
    // Cmd+click is needed before the range works.
    await rows(page).nth(0).click();
    await expect.poll(() => page.url()).toContain("/session/s-1");
    await rows(page)
      .nth(2)
      .click({ modifiers: ["Shift"] });
    await expect(selectedRows(page)).toHaveCount(3);
  });
});

// #2252: the Switch view item opens a capability-aware confirm dialog, then
// POSTs acp enable/disable. The backend round-trip is covered by Rust and live
// specs; this pins the menu presence, the confirm gate, and each request.
test.describe("Sidebar Switch view (#2252)", () => {
  const session = (id: string, title: string, view: "structured" | "terminal", acpCapable: boolean) => ({
    id,
    title,
    project_path: "/tmp/repo",
    group: "/tmp/repo",
    branch: null,
    fields: {
      acp_agent: "claude",
      view,
      acp_capable: acpCapable,
      // Server-computed context-preservation gate: `claude` resumes its
      // conversation in both directions, so the daemon reports true here.
      keeps_context: true,
      smart_rename: "inactive",
      default_name: false,
    },
  });

  async function openSwitchMenu(page: Page, title: string) {
    await page.goto("/");
    await rows(page).filter({ hasText: title }).first().click({ button: "right" });
    await expect(menu(page)).toBeVisible();
    await page.locator("[data-testid='sidebar-context-menu-switch-view']").click();
  }

  test("switches each way after confirm; a non-acp-capable session has no item", async ({ page }) => {
    await installSidebarMocks(page, {
      sessions: [
        session("sess-1", "Switch sess-1", "structured", true),
        session("sess-2", "Switch sess-2", "terminal", true),
        session("sess-3", "Plain terminal", "terminal", false),
      ],
    });
    const posted: string[] = [];
    await page.route("**/api/sessions/*/acp/*", (r) => {
      const m = r
        .request()
        .url()
        .match(/\/api\/sessions\/([^/]+)\/acp\/(enable|disable)$/);
      if (!m || r.request().method() !== "POST") return r.fulfill({ status: 400 });
      posted.push(`${m[1]}/${m[2]}`);
      return r.fulfill({ json: { session_id: m[1], view: m[2] === "enable" ? "structured" : "terminal" } });
    });

    await openSwitchMenu(page, "Switch sess-1");
    const dialog = page.locator("[data-testid='switch-view-dialog']");
    await expect(dialog).toBeVisible();
    // Claude keeps context: the confirm copy must say so, not threaten loss.
    await expect(dialog).toContainText("continues in the terminal");
    await page.locator("[data-testid='switch-view-confirm']").click();
    await expect.poll(() => posted).toEqual(["sess-1/disable"]);
    await expect(page.getByText("Switched to terminal")).toBeVisible();

    await rows(page).filter({ hasText: "Switch sess-2" }).first().click({ button: "right" });
    await page.locator("[data-testid='sidebar-context-menu-switch-view']").click();
    await page.locator("[data-testid='switch-view-confirm']").click();
    await expect.poll(() => posted).toEqual(["sess-1/disable", "sess-2/enable"]);

    await rows(page).filter({ hasText: "Plain terminal" }).first().click({ button: "right" });
    await expect(menu(page)).toBeVisible();
    await expect(page.locator("[data-testid='sidebar-context-menu-switch-view']")).toHaveCount(0);
  });

  test("failed switches to terminal surface the refusal guidance, else the generic error", async ({ page }) => {
    await installSidebarMocks(page, {
      sessions: [session("sess-9", "Refused handoff", "structured", true)],
    });
    const guidance =
      "Native store is unknown. Run aoe session set-session-id sess-9 conversation-id --store /alternate/claude to restore context.";
    const failures = [
      (r: Route) => r.fulfill({ status: 409, contentType: "text/plain", body: guidance }),
      (r: Route) => r.fulfill({ status: 500 }),
      (r: Route) => r.abort("failed"),
    ];
    await page.route("**/api/sessions/*/acp/disable", (r) => failures.shift()!(r));

    await openSwitchMenu(page, "Refused handoff");
    await page.locator("[data-testid='switch-view-confirm']").click();
    await expect(page.getByRole("alert").filter({ hasText: guidance })).toBeVisible();
    await expect(page.locator("[data-testid='switch-view-dialog']")).toBeHidden();

    // A 500 with no body, then a network failure: each adds one generic toast
    // (both land well inside the toast lifetime).
    const generic = page.getByText("Failed to switch to terminal", { exact: true });
    for (const shown of [1, 2]) {
      await rows(page).filter({ hasText: "Refused handoff" }).first().click({ button: "right" });
      await page.locator("[data-testid='sidebar-context-menu-switch-view']").click();
      await page.locator("[data-testid='switch-view-confirm']").click();
      await expect(generic).toHaveCount(shown);
    }
  });

  test("a failed switch to structured view keeps the terminal available", async ({ page }) => {
    await installSidebarMocks(page, {
      sessions: [session("sess-9", "Cannot enable", "terminal", true)],
    });
    await page.route("**/api/sessions/*/acp/enable", (r) => r.fulfill({ status: 500 }));

    await openSwitchMenu(page, "Cannot enable");
    await page.locator("[data-testid='switch-view-confirm']").click();
    await expect(page.getByText("Failed to switch to structured view")).toBeVisible();
    await expect(page.locator("[data-testid='switch-view-dialog']")).toBeHidden();

    await rows(page).filter({ hasText: "Cannot enable" }).first().click({ button: "right" });
    await expect(page.locator("[data-testid='sidebar-context-menu-switch-view']")).toContainText("structured");
  });
});

// The `ctx:no` row badge marks sessions whose next launch cannot resume
// context. Only the unavailable states are surfaced.
test.describe("Context resume badge", () => {
  const session = (id: string, title: string, fields: Record<string, unknown> = {}): MockSessionInput => ({
    id,
    title,
    project_path: `/tmp/${id}`,
    branch: null,
    created_at: "2026-09-04T00:00:00Z",
    fields,
  });

  test("surfaces only unavailable context resume states", async ({ page }) => {
    await openSidebar(page, [
      session("missing", "Missing target", {
        context_resume: { state: "unavailable", reason: "no_target" },
      }),
      session("runtime", "Runtime check", {
        context_resume: {
          state: "indeterminate",
          reason: "runtime_check_required",
        },
      }),
      session("available", "Available context", {
        context_resume: { state: "available" },
      }),
      session("old-daemon", "Unreported context"),
      session("future-reason", "Future reason", {
        context_resume: { state: "unavailable", reason: "future_reason" },
      }),
      session("future-state", "Future state", {
        context_resume: { state: "future_state" },
      }),
    ]);

    await expect(page.getByTitle("Context resume unavailable: no resume target has been captured")).toHaveText(
      "ctx:no",
    );
    await expect(page.getByTitle("Context resume unavailable", { exact: true })).toHaveText("ctx:no");
    await expect(page.getByRole("link", { name: /Missing target/ })).toHaveAccessibleName(/Missing target ctx:no$/);
    for (const title of ["Runtime check", "Available context", "Unreported context", "Future state"]) {
      await expect(page.getByRole("link", { name: new RegExp(title) })).not.toContainText("ctx:");
    }
  });

  test("uses the active session for a multi-session workspace badge and navigation", async ({ page }) => {
    await openSidebar(page, [
      {
        ...session("idle", "Idle session", {
          status: "Idle",
          context_resume: { state: "unavailable", reason: "no_target" },
        }),
        project_path: "/tmp/shared",
        branch: "feature/shared",
      },
      {
        ...session("running", "Running session", {
          status: "Running",
          context_resume: { state: "unavailable", reason: "forced_fresh" },
        }),
        project_path: "/tmp/shared",
        branch: "feature/shared",
      },
    ]);

    await expect(page.getByTitle("Context resume unavailable: the next launch was explicitly reset")).toHaveText(
      "ctx:no",
    );
    await expect(page.getByTitle("Context resume unavailable: no resume target has been captured")).toHaveCount(0);
    const workspaceLink = page.locator('a[href="/session/running"]');
    await expect(workspaceLink).toHaveCount(1);
    await workspaceLink.click();
    await expect(page).toHaveURL(/\/session\/running$/);
  });

  for (const axis of ["group", "repo+group"]) {
    test(`keeps the badge and activation on the same ${axis} slice`, async ({ page }) => {
      await page.addInitScript((axis) => localStorage.setItem("aoe-sidebar-axis", axis), axis);
      await page.route("**/api/app-state/web-ui-state", (r) => r.fulfill({ json: { "aoe-sidebar-axis": axis } }));
      await openSidebar(page, [
        {
          ...session("idle", "Idle session", {
            status: "Stopped",
            context_resume: { state: "unavailable", reason: "no_target" },
          }),
          project_path: "/tmp/shared",
          branch: "shared",
          group: "A",
        },
        {
          ...session("running", "Running session", {
            status: "Running",
            context_resume: { state: "available" },
          }),
          project_path: "/tmp/shared",
          branch: "shared",
          group: "B",
        },
      ]);

      const row = page.locator('a[href="/session/idle"]');
      await expect(row).toContainText("ctx:no");
      await row.click({ modifiers: ["Control"] });
      await expect(page).toHaveURL(/\/$/);
      await expect(row).toHaveAttribute("data-selected", "true");
      await row.click();
      await expect(page).toHaveURL(/\/session\/idle$/);
      await page.goto("/");
      await row.focus();
      await page.keyboard.press("Enter");
      await expect(page).toHaveURL(/\/session\/idle$/);
    });
  }
});

// #3460: Android fires a native contextmenu after the long-press timer has
// already opened the menu, and that must not dismiss it. Chromium emits none on
// a touch hold, so a CDP hold arms the timer and the event is synthesized at
// both plausible targets: the menu under the finger, and the row.
test.describe("Long-press menu (mobile)", () => {
  test.use(iPhone13);

  const LONG_PRESS_MS = 500;

  test("a native contextmenu after the long-press does not dismiss the row menu", async ({ page }) => {
    await installSidebarMocks(page, { sessions: threeSessionsInOneRepo() });
    await page.goto("/");
    await openMobileSidebar(page);

    const row = rows(page).first();
    await expect(row).toBeVisible();
    const box = (await row.boundingBox())!;
    const [x, y] = [box.x + box.width / 2, box.y + box.height / 2];

    const cdp = await page.context().newCDPSession(page);
    await cdp.send("Input.dispatchTouchEvent", {
      type: "touchStart",
      touchPoints: [{ x, y, id: 1 }],
    });
    // The guard window starts at open; expect's backoff alone can spend most of it.
    await page.waitForFunction((sel) => document.querySelector(sel) !== null, MENU, { polling: "raf" });
    await expect(menu(page)).toBeVisible();

    // The menu sits under the finger, which is what makes the inside-menu guard matter.
    const topmostIsMenu = await page.evaluate(
      ({ px, py, sel }) => {
        const target = document.elementFromPoint(px, py);
        const el = document.querySelector(sel);
        return !!target && !!el && el.contains(target);
      },
      { px: x, py: y, sel: MENU },
    );
    expect(topmostIsMenu).toBe(true);

    await page.evaluate(
      ({ px, py }) =>
        document.elementFromPoint(px, py)?.dispatchEvent(
          new MouseEvent("contextmenu", {
            bubbles: true,
            cancelable: true,
            composed: true,
            button: 2,
            clientX: px,
            clientY: py,
          }),
        ),
      { px: x, py: y },
    );
    await expect(menu(page)).toBeVisible();

    await row.evaluate(
      (el, { px, py }) =>
        el.dispatchEvent(
          new MouseEvent("contextmenu", {
            bubbles: true,
            cancelable: true,
            composed: true,
            button: 2,
            clientX: px,
            clientY: py,
          }),
        ),
      { px: x, py: y },
    );
    await expect(menu(page)).toBeVisible();

    await cdp.send("Input.dispatchTouchEvent", {
      type: "touchEnd",
      touchPoints: [],
    });

    // The guard is a time window: later, a tap outside still dismisses. Tap
    // beside the menu, which is nearly full height on a phone and so attracts
    // near-miss taps above or below.
    await page.waitForTimeout(LONG_PRESS_MS + 100);
    const outside = await menu(page).evaluate((el) => {
      const r = el.getBoundingClientRect();
      const gapLeft = r.left;
      const gapRight = window.innerWidth - r.right;
      if (Math.max(gapLeft, gapRight) < 12) throw new Error("no horizontal gap beside the menu");
      return {
        x: Math.round(gapLeft >= gapRight ? gapLeft / 2 : (r.right + window.innerWidth) / 2),
        y: Math.round(r.top + r.height / 2),
      };
    });
    const urlBefore = page.url();
    await page.touchscreen.tap(outside.x, outside.y);
    await expect(menu(page)).toBeHidden();
    // Dismissed by the document listener, not by navigating.
    expect(page.url()).toBe(urlBefore);
  });
});
