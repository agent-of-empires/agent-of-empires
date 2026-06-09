import { test, expect } from "./helpers/mockedTest";

const NEW_SESSION_PANE_NAME = /New session Pick a project, then launch a new session/i;

// Verifies URL-based routing: deep links land on the right view, refresh
// preserves location, and back/forward replays history.
test.describe("URL routing", () => {
  test("'/' renders the dashboard home screen", async ({ page }) => {
    await page.goto("/");
    await expect(page.getByRole("button", { name: NEW_SESSION_PANE_NAME })).toBeVisible();
    await expect(page).toHaveURL("/");
  });

  test("'/settings' renders settings on first load", async ({ page }) => {
    await page.goto("/settings");
    await expect(page.getByText("Settings", { exact: true }).first()).toBeVisible();
    await expect(page).toHaveURL("/settings");
  });

  test("settings tab is reflected in the URL", async ({ page }) => {
    await page.goto("/settings/theme");
    await expect(page.getByRole("heading", { name: "Theme" })).toBeVisible();
    await expect(page).toHaveURL("/settings/theme");
  });

  test("refresh on /settings keeps user on settings", async ({ page }) => {
    await page.goto("/settings");
    await expect(page.getByText("Settings", { exact: true }).first()).toBeVisible();
    await page.reload();
    await expect(page.getByText("Settings", { exact: true }).first()).toBeVisible();
    await expect(page).toHaveURL("/settings");
  });

  test("'/session/<id>' for an unknown session falls back to dashboard", async ({ page }) => {
    // No backend, sessions list is empty, so the route still matches but
    // the resolver finds no session and the dashboard renders. Importantly
    // the URL stays put so a real backend can later resolve it.
    await page.goto("/session/does-not-exist");
    await expect(page.getByRole("button", { name: NEW_SESSION_PANE_NAME })).toBeVisible();
    await expect(page).toHaveURL("/session/does-not-exist");
  });

  test("'/session/<id>' holds the loading shell while the sessions list is still in flight", async ({ page }) => {
    // Hold the sessions list open for 1s before responding so the
    // App.tsx render gate added for #1351 (the `!sessionsLoaded`
    // branch) executes for long enough to be observable. Without the
    // gate the dashboard fallback would render immediately and the
    // dark-shell assertion below would fail. After the response lands
    // the dashboard fallback takes over because the response carries
    // no matching session.
    await page.route("**/api/sessions", async (r) => {
      if (r.request().method() === "POST") return r.fulfill({ status: 400 });
      await new Promise((resolve) => setTimeout(resolve, 1000));
      await r.fulfill({
        json: { sessions: [], workspace_ordering: [] },
      });
    });

    await page.goto("/session/loading-window");
    // The minimal pre-auth shell is just a dark <div> with no text or
    // role-bearing children, so the assertion negates the dashboard
    // CTA which is what would otherwise render in this window.
    await expect(page.getByRole("button", { name: NEW_SESSION_PANE_NAME })).not.toBeVisible();
    // After the stubbed response lands the dashboard fallback takes
    // over, since the response carries no matching session.
    await expect(page.getByRole("button", { name: NEW_SESSION_PANE_NAME })).toBeVisible();
    await expect(page).toHaveURL("/session/loading-window");
  });

  test("refresh on /session/<id> for a known session keeps the user on that session", async ({ page }) => {
    // Stub the sessions list with a single known session, then visit
    // /session/<id>, reload, and assert the dashboard fallback never
    // shows. This pins #1351: before the fix the dashboard would flash
    // on every refresh during the brief window where `useSessions` had
    // not yet resolved its first fetch.
    await page.route("**/api/sessions", (r) => {
      if (r.request().method() === "POST") return r.fulfill({ status: 400 });
      return r.fulfill({
        json: {
          sessions: [
            {
              id: "known-session",
              title: "known-session",
              project_path: "/tmp/known",
              group_path: "/tmp",
              tool: "claude",
              status: "Running",
              yolo_mode: false,
              created_at: new Date().toISOString(),
              last_accessed_at: null,
              idle_entered_at: null,
              last_error: null,
              branch: null,
              main_repo_path: null,
              is_sandboxed: false,
              has_managed_worktree: false,
              has_terminal: true,
              profile: "default",
              cleanup_defaults: {},
              remote_owner: null,
              notify_on_waiting: null,
              notify_on_idle: null,
              notify_on_error: null,
              claude_fullscreen: false,
              workspace_repos: [],
            },
          ],
          workspace_ordering: [],
        },
      });
    });
    await page.route("**/api/sessions/*/ensure", (r) => r.fulfill({ json: { ok: true } }));
    await page.route("**/api/sessions/*/terminal", (r) => r.fulfill({ status: 200, body: "" }));
    await page.routeWebSocket(/\/sessions\/.*\/(ws|acp-ws)$/, () => {});

    await page.goto("/session/known-session");
    await expect(page).toHaveURL("/session/known-session");
    await expect(page.getByRole("button", { name: NEW_SESSION_PANE_NAME })).not.toBeVisible();

    await page.reload();
    await expect(page).toHaveURL("/session/known-session");
    await expect(page.getByRole("button", { name: NEW_SESSION_PANE_NAME })).not.toBeVisible();
  });

  test("legacy '?session=X' URL is rewritten to '/session/X'", async ({ page }) => {
    await page.goto("/?session=abc-123");
    await expect(page).toHaveURL("/session/abc-123");
  });

  test("browser back navigates dashboard ↔ settings", async ({ page }) => {
    await page.goto("/");
    await page.goto("/settings");
    await expect(page).toHaveURL("/settings");
    await page.goBack();
    await expect(page).toHaveURL("/");
    await page.goForward();
    await expect(page).toHaveURL("/settings");
  });
});
