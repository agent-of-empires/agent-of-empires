// Focus mode (`/session/<id>?focus=1`): a window showing only one Session, entered in place from the Session menu.

import type { Page } from "@playwright/test";
import { test, expect } from "./helpers/mockedTest";
import { installSidebarMocks, type MockSessionInput } from "./helpers/sidebarMocks";
import { sessionResponse } from "./helpers/sessions";

const SIDEBAR = '[role="navigation"][aria-label="Sessions sidebar"]';

const SESSIONS: MockSessionInput[] = [
  { id: "s-1", title: "Mongols", project_path: "/tmp/repo-a", branch: null },
  { id: "s-2", title: "Goths", project_path: "/tmp/repo-b", branch: null },
];

/** Records the focus session each presence report carries. */
async function recordPresence(page: Page): Promise<(string | null)[]> {
  const reports: (string | null)[] = [];
  await page.route("**/api/presence", async (route) => {
    const body = route.request().postDataJSON() as { focus_session: string | null };
    reports.push(body.focus_session);
    await route.fulfill({ status: 204 });
  });
  return reports;
}

/** The focus session the service worker's push and click routing reads for each open window. */
async function workerFocusSessions(page: Page): Promise<(string | null)[]> {
  const context = page.context();
  const worker = context.serviceWorkers()[0] ?? (await context.waitForEvent("serviceworker"));
  return worker.evaluate(async () => {
    const sw = self as unknown as {
      clients: { matchAll: (options: object) => Promise<{ url: string }[]> };
      focusSessionOf: (client: { url: string }) => string | null;
    };
    const clients = await sw.clients.matchAll({ type: "window", includeUncontrolled: true });
    return clients.map((client) => sw.focusSessionOf(client));
  });
}

test("Show only this Session turns this tab into a focus window until All Sessions", async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 720 });
  await installSidebarMocks(page, { sessions: SESSIONS });
  const reports = await recordPresence(page);
  const popups: Page[] = [];
  page.on("popup", (p) => popups.push(p));
  await page.goto("/session/s-1");
  await expect(page.locator(SIDEBAR)).toBeVisible();
  await expect.poll(() => workerFocusSessions(page)).toEqual([null]);
  const appTitle = await page.title();

  await page.locator(SIDEBAR).getByTestId("sidebar-session-row").filter({ hasText: "Mongols" }).click({
    button: "right",
  });
  await page.getByTestId("sidebar-context-menu-more").click();
  await page.getByTestId("sidebar-context-menu-show-only").click();
  await expect(page).toHaveURL("/session/s-1?focus=1");
  const exit = page.getByRole("button", { name: /^All Sessions/ });
  await expect(exit).toBeVisible();
  await expect(page.locator(SIDEBAR)).toHaveCount(0);
  await expect(page.getByRole("button", { name: "Go to dashboard" })).toHaveCount(0);
  await expect(page).toHaveTitle("Mongols");
  await expect.poll(() => reports.at(-1)).toBe("s-1");
  // The service worker scopes pushes and notification clicks by the window's URL, so it must see the change.
  await expect.poll(() => workerFocusSessions(page)).toEqual(["s-1"]);

  // The first-use note names the session and stays dismissed.
  const note = page.getByTestId("focus-mode-note");
  await expect(note).toContainText("Only Mongols shows here.");
  await note.getByRole("button", { name: "Dismiss" }).click();
  await expect(note).toHaveCount(0);

  // A reload keeps focus mode and the dismissed note, and the sidebar shortcut does nothing.
  await page.reload();
  await expect(exit).toBeVisible();
  await expect(note).toHaveCount(0);
  await page.keyboard.press("ControlOrMeta+b");
  await expect(page.locator(SIDEBAR)).toHaveCount(0);

  await exit.click();
  await expect(page).toHaveURL("/session/s-1");
  await expect(page.locator(SIDEBAR)).toBeVisible();
  await expect(page).toHaveTitle(appTitle);
  await expect.poll(() => reports.at(-1)).toBeNull();
  await expect.poll(() => workerFocusSessions(page)).toEqual([null]);
  expect(popups).toHaveLength(0);
});

/** Clicks `trigger` and returns the path of the window it opens. */
async function popupPath(page: Page, trigger: () => Promise<void>): Promise<string> {
  const [popup] = await Promise.all([page.waitForEvent("popup"), trigger()]);
  await popup.waitForURL((url) => url.pathname !== "blank");
  const url = new URL(popup.url());
  await popup.close();
  return url.pathname + url.search;
}

test("a focus window opens other sessions, plugin pages and settings beside it and keeps its own URL", async ({
  page,
}) => {
  await page.setViewportSize({ width: 1280, height: 720 });
  await installSidebarMocks(page, { sessions: SESSIONS });
  const board = { plugin_id: "acme.kit", id: "board", title: "Board", icon: "kanban", capabilities: [] };
  // A header badge linking to another session, which a plugin may show in any window.
  const link = {
    plugin_id: "acme.kit",
    slot: "header-start",
    id: "goths",
    payload: { text: "acme: goths", href: "/session/s-2" },
  };
  await page.route("**/api/plugins/ui-state", (r) =>
    r.fulfill({ json: { entries: [link], notifications: [], pages: [board] } }),
  );
  await page.goto("/session/s-1?focus=1");
  await expect(page.getByRole("button", { name: /^All Sessions/ })).toBeVisible();

  expect(await popupPath(page, () => page.getByRole("link", { name: "acme: goths", exact: true }).click())).toBe(
    "/session/s-2",
  );
  expect(await popupPath(page, () => page.getByRole("button", { name: "Board", exact: true }).click())).toBe(
    "/plugins/acme.kit/board",
  );
  expect(await popupPath(page, () => page.keyboard.press("s"))).toBe("/settings");
  await expect(page).toHaveURL("/session/s-1?focus=1");
  await expect(page.getByRole("button", { name: /^All Sessions/ })).toBeVisible();
});

test("a focus window names its session once it is gone and leads back to all Sessions", async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 720 });
  let sessions = SESSIONS;
  await installSidebarMocks(page, { sessions: SESSIONS });
  await page.route("**/api/sessions", (r) =>
    r.fulfill({
      headers: { "aoe-runtime-epoch": "mock-runtime", "aoe-runtime-revision": "1" },
      json: {
        sessions: sessions.map((s, i) => ({
          ...sessionResponse({ id: s.id, title: s.title, project_path: s.project_path, branch: s.branch }),
          created_at: new Date(Date.UTC(2025, 0, 1 + i)).toISOString(),
        })),
        workspace_ordering: [],
      },
    }),
  );
  await page.goto("/session/s-1?focus=1");
  await expect(page).toHaveTitle("Mongols");

  sessions = SESSIONS.filter((s) => s.id !== "s-1");
  const gone = page.getByTestId("focus-session-gone");
  await expect(gone).toContainText("Mongols no longer exists.");
  await expect(page).toHaveURL("/session/s-1?focus=1");

  await gone.getByRole("button", { name: "Show all Sessions" }).click();
  await expect(page).toHaveURL("/");
  await expect(page.locator(SIDEBAR)).toBeVisible();
  await expect(gone).toHaveCount(0);
});

test("Open in its own window pops the session out and leaves this tab as it was", async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 720 });
  await installSidebarMocks(page, { sessions: SESSIONS });
  await page.goto("/session/s-1");

  await page.locator(SIDEBAR).getByTestId("sidebar-session-row").filter({ hasText: "Goths" }).click({
    button: "right",
  });
  await page.getByTestId("sidebar-context-menu-more").click();
  const [popup] = await Promise.all([
    page.waitForEvent("popup"),
    page.getByTestId("sidebar-context-menu-own-window").click(),
  ]);
  await expect(popup).toHaveURL(/\/session\/s-2\?focus=1$/);
  await expect(page).toHaveURL("/session/s-1");
  await expect(page.locator(SIDEBAR)).toBeVisible();
});
