// Focus mode (`/session/<id>?focus=1`): a window limited to one session, entered in place from the top bar.

import type { Page } from "@playwright/test";
import { test, expect } from "./helpers/mockedTest";
import { installSidebarMocks, type MockSessionInput } from "./helpers/sidebarMocks";

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

test("the top bar turns this tab into a focus window that hides the sidebar until it exits", async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 720 });
  await installSidebarMocks(page, { sessions: SESSIONS });
  const reports = await recordPresence(page);
  const popups: Page[] = [];
  page.on("popup", (p) => popups.push(p));
  await page.goto("/session/s-1");
  await expect(page.locator(SIDEBAR)).toBeVisible();
  await expect.poll(() => workerFocusSessions(page)).toEqual([null]);

  await page.getByTestId("topbar-enter-focus").click();
  await expect(page).toHaveURL("/session/s-1?focus=1");
  const exit = page.getByTestId("topbar-exit-focus");
  await expect(exit).toBeVisible();
  await expect(page.locator(SIDEBAR)).toHaveCount(0);
  await expect(page.getByTestId("topbar-enter-focus")).toHaveCount(0);
  await expect(page.getByRole("button", { name: "Go to dashboard" })).toHaveCount(0);
  await expect.poll(() => reports.at(-1)).toBe("s-1");
  // The service worker scopes pushes and notification clicks by the window's URL, so it must see the change.
  await expect.poll(() => workerFocusSessions(page)).toEqual(["s-1"]);

  // A reload keeps focus mode, and the sidebar shortcut does nothing.
  await page.reload();
  await expect(exit).toBeVisible();
  await page.keyboard.press("ControlOrMeta+b");
  await expect(page.locator(SIDEBAR)).toHaveCount(0);

  await exit.click();
  await expect(page).toHaveURL("/session/s-1");
  await expect(page.locator(SIDEBAR)).toBeVisible();
  await expect.poll(() => reports.at(-1)).toBeNull();
  await expect.poll(() => workerFocusSessions(page)).toEqual([null]);
  expect(popups).toHaveLength(0);
});
