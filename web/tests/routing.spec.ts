import { test, expect } from "./helpers/mockedTest";
import { sessionResponse } from "./helpers/sessions";

function makeSession(id: string, projectPath = `/tmp/${id}`) {
  return sessionResponse({
    id,
    project_path: projectPath,
    group_path: "/tmp",
    status: "Running",
    has_managed_worktree: false,
    cleanup_defaults: {},
    remote_owner: null,
    notify_on_waiting: null,
    notify_on_idle: null,
    notify_on_error: null,
    claude_fullscreen: false,
  });
}

const NEW_SESSION_PANE_NAME = /New session Pick a project, then launch a new session/i;

// Verifies URL-based routing: deep links land on the right view, refresh
// preserves location, and back/forward replays history.
test.describe("URL routing", () => {
  test("settings deep links, tab URLs, refresh, and back/forward", async ({ page }) => {
    await page.goto("/");
    await page.goto("/settings");
    await expect(page.getByText("Settings", { exact: true }).first()).toBeVisible();
    await expect(page).toHaveURL("/settings");

    await page.goto("/settings/theme");
    await expect(page.getByRole("heading", { name: "Theme" })).toBeVisible();
    await page.reload();
    await expect(page.getByRole("heading", { name: "Theme" })).toBeVisible();
    await expect(page).toHaveURL("/settings/theme");

    await page.goBack();
    await expect(page).toHaveURL("/settings");
    await page.goBack();
    await expect(page).toHaveURL("/");
    await page.goForward();
    await expect(page).toHaveURL("/settings");
  });

  test("an unknown or legacy '?session=' session URL keeps its '/session/<id>' path over the dashboard", async ({
    page,
  }) => {
    // No backend, sessions list is empty, so the route still matches but
    // the resolver finds no session and the dashboard renders. Importantly
    // the URL stays put so a real backend can later resolve it.
    await page.goto("/session/does-not-exist");
    await expect(page.getByRole("button", { name: NEW_SESSION_PANE_NAME })).toBeVisible();
    await expect(page).toHaveURL("/session/does-not-exist");

    // A legacy '?session=X' URL is rewritten to '/session/X'.
    await page.goto("/?session=abc-123");
    await expect(page).toHaveURL("/session/abc-123");
  });

  test("'/session/<id>' holds the loading shell while the sessions list is still in flight", async ({ page }) => {
    let releaseSessions!: () => void;
    const sessionsPending = new Promise<void>((resolve) => {
      releaseSessions = resolve;
    });
    let requested = false;
    await page.route("**/api/sessions", async (route) => {
      if (route.request().method() === "POST") return route.fulfill({ status: 400 });
      requested = true;
      await sessionsPending;
      await route.fulfill({ json: { sessions: [], workspace_ordering: [] } });
    });

    try {
      await page.goto("/session/loading-window");
      await expect.poll(() => requested).toBe(true);
      // The shell must have passed the separate /about gate before this negative assertion.
      await expect(page.locator("header")).toBeVisible();
      await expect(page.getByRole("button", { name: NEW_SESSION_PANE_NAME })).not.toBeVisible();
    } finally {
      releaseSessions();
    }
    await expect(page.getByRole("button", { name: NEW_SESSION_PANE_NAME })).toBeVisible();
    await expect(page).toHaveURL("/session/loading-window");
  });

  test("refresh on /session/<id> for a known session keeps the user on that session", async ({ page }) => {
    // The known session must resolve to its terminal on both navigations.
    await page.route("**/api/sessions", (r) => {
      if (r.request().method() === "POST") return r.fulfill({ status: 400 });
      return r.fulfill({
        json: {
          sessions: [makeSession("known-session", "/tmp/known")],
          workspace_ordering: [],
        },
      });
    });
    await page.route("**/api/sessions/*/ensure", (r) => r.fulfill({ json: { ok: true } }));
    await page.route("**/api/sessions/*/terminal", (r) => r.fulfill({ status: 200, body: "" }));
    await page.routeWebSocket(/\/sessions\/.*\/(?:live-ws|ws|acp-ws)(?:\?.*)?$/, () => {});

    await page.goto("/session/known-session");
    await expect(page.locator('[data-term="agent"] [data-live-terminal]')).toBeVisible();
    await expect(page.locator('[data-term="agent"] textarea')).toHaveCount(1);
    await expect(page).toHaveURL("/session/known-session");
    await expect(page.getByRole("button", { name: NEW_SESSION_PANE_NAME })).not.toBeVisible();

    await page.reload();
    await expect(page.locator('[data-term="agent"] [data-live-terminal]')).toBeVisible();
    await expect(page.locator('[data-term="agent"] textarea')).toHaveCount(1);
    await expect(page).toHaveURL("/session/known-session");
    await expect(page.getByRole("button", { name: NEW_SESSION_PANE_NAME })).not.toBeVisible();
  });
});

const LAST_SESSION_KEY = "aoe-last-session-id";

async function stubSessions(page: import("@playwright/test").Page, ids: string[]) {
  await page.route("**/api/sessions", (r) => {
    if (r.request().method() === "POST") return r.fulfill({ status: 400 });
    return r.fulfill({ json: { sessions: ids.map((id) => makeSession(id)), workspace_ordering: [] } });
  });
  await page.route("**/api/sessions/*/ensure", (r) => r.fulfill({ json: { ok: true } }));
  await page.route("**/api/sessions/*/terminal", (r) => r.fulfill({ status: 200, body: "" }));
  await page.routeWebSocket(/\/sessions\/.*\/(ws|acp-ws)$/, () => {});
}

// Verifies the PWA reopens to the session the user last had open (#2103).
test.describe("PWA last-session restore", () => {
  // Restore is gated on isStandalone() (a plain browser tab must never bounce
  // off the dashboard), so this suite forces the standalone media query to
  // deterministically behave like an installed PWA, matching the describe title.
  test.beforeEach(async ({ page }) => {
    await page.addInitScript(() => {
      const orig = window.matchMedia.bind(window);
      window.matchMedia = ((query: string) => {
        if (query === "(display-mode: standalone)") {
          return {
            matches: true,
            media: query,
            onchange: null,
            addListener: () => {},
            removeListener: () => {},
            addEventListener: () => {},
            removeEventListener: () => {},
            dispatchEvent: () => false,
          } as unknown as MediaQueryList;
        }
        return orig(query);
      }) as typeof window.matchMedia;
    });
  });

  test("cold launch to '/' restores the stored last session", async ({ page }) => {
    await stubSessions(page, ["known-session"]);
    await page.addInitScript(
      ([key, id]) => {
        try {
          localStorage.setItem(key, id);
        } catch {
          // storage disabled; the app degrades to no-restore
        }
      },
      [LAST_SESSION_KEY, "known-session"],
    );

    await page.goto("/");
    await expect(page).toHaveURL("/session/known-session");
    await expect(page.getByRole("button", { name: NEW_SESSION_PANE_NAME })).not.toBeVisible();
  });
});
