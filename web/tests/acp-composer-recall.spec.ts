import { test, expect, type Page } from "./helpers/mockedTest";
import { clickSidebarSession, openMobileSidebar } from "./helpers/sidebar";

// Queue-recall behavior for the structured-view composer (#2147), driven
// through the real component in mocked mode so the ArrowUp/ArrowDown
// handlers, the "Editing queued message" banner, Esc-restore, and the
// edit-in-place submit path all execute in the browser.
//
// Sending the first prompt flips the session turn-active (optimistic
// user_prompt dispatch), so subsequent submissions park in the queue
// rather than sending. From there the arrows browse the queue.

const SESSION_ID = "sess-acp-recall";
const TITLE = "acp-recall";

async function setup(page: Page) {
  await page.route("**/api/login/status", (r) => r.fulfill({ json: { required: false, authenticated: true } }));
  for (const path of [
    "settings",
    "themes",
    "agents",
    "profiles",
    "groups",
    "devices",
    "docker/status",
    "about",
    "system/update-status",
  ]) {
    await page.route(`**/api/${path}`, (r) =>
      r.fulfill({
        json:
          path === "docker/status" || path === "about" || path === "settings" || path === "system/update-status"
            ? {}
            : [],
      }),
    );
  }
  await page.route("**/api/sessions", (r) => {
    if (r.request().method() === "POST") return r.fulfill({ status: 400 });
    return r.fulfill({
      json: {
        sessions: [
          {
            id: SESSION_ID,
            title: TITLE,
            project_path: "/tmp/acp-recall",
            group_path: "/tmp",
            tool: "claude",
            status: "Running",
            yolo_mode: false,
            created_at: new Date().toISOString(),
            last_accessed_at: null,
            last_error: null,
            branch: null,
            main_repo_path: null,
            is_sandboxed: false,
            has_terminal: true,
            profile: "default",
            workspace_repos: [],
            view: "structured",
            acp_worker_state: "running",
            claude_fullscreen: false,
          },
        ],
        workspace_ordering: [],
      },
    });
  });
  await page.route("**/api/sessions/*/ensure", (r) => r.fulfill({ json: { ok: true } }));
  // Prompt POSTs + replay succeed (empty), so the optimistic send sticks.
  await page.route("**/api/sessions/*/acp/**", (r) => r.fulfill({ json: {} }));
  // Accept both sockets as open-but-silent: the session reads as connected
  // so the first Enter sends (turn active) and the rest queue.
  await page.routeWebSocket(/\/sessions\/[^/]+\/ws(\?|$)/, () => {});
  await page.routeWebSocket(/\/sessions\/[^/]+\/acp\/ws/, () => {});
}

async function openStructuredSession(page: Page) {
  await page.goto("/");
  await expect(page.locator("header")).toBeVisible();
  await openMobileSidebar(page);
  await clickSidebarSession(page, TITLE);
  await expect(page.getByTestId("structured-view-root")).toBeVisible({ timeout: 10000 });
}

test.describe("Structured-view composer queue recall (#2147)", () => {
  test("ArrowUp recalls queued prompts, banner + Esc + edit-in-place", async ({ page }) => {
    await setup(page);
    await openStructuredSession(page);

    const composer = page.getByRole("textbox", {
      name: /Send a message|Queue a follow-up/i,
    });

    // Send the first prompt to put the turn active, then queue two follow-ups.
    await composer.fill("kick off");
    await composer.press("Enter");
    const queueButton = page.getByRole("button", { name: /Queue follow-up message/i });
    await expect(queueButton).toBeVisible({ timeout: 10000 });

    await composer.fill("first queued");
    await queueButton.click();
    await composer.fill("second queued");
    await queueButton.click();
    await expect(page.getByRole("button", { name: /^second queued$/ })).toBeVisible({ timeout: 5000 });

    // Empty composer: ArrowUp enters recall on the newest, banner shows.
    await expect(composer).toHaveValue("");
    await composer.press("ArrowUp");
    await expect(composer).toHaveValue("second queued");
    await expect(page.getByText(/Editing queued message 1 of 2/)).toBeVisible();

    // ArrowDown past the newest restores the stashed (empty) draft and exits.
    await composer.press("ArrowDown");
    await expect(composer).toHaveValue("");
    await expect(page.getByText(/Editing queued message/)).toHaveCount(0);

    // Re-enter and walk to the oldest.
    await composer.press("ArrowUp");
    await expect(composer).toHaveValue("second queued");
    await composer.press("ArrowUp");
    await expect(composer).toHaveValue("first queued");
    await expect(page.getByText(/Editing queued message 2 of 2/)).toBeVisible();

    // ArrowUp at the oldest is a no-op (no wrap): stays on the oldest.
    await composer.press("ArrowUp");
    await expect(composer).toHaveValue("first queued");
    await expect(page.getByText(/Editing queued message 2 of 2/)).toBeVisible();

    await composer.press("ArrowDown");
    await expect(composer).toHaveValue("second queued");

    // Esc restores the stashed (empty) draft and clears the banner.
    await composer.press("Escape");
    await expect(composer).toHaveValue("");
    await expect(page.getByText(/Editing queued message/)).toHaveCount(0);

    // Re-enter, edit, submit: the queued entry updates in place, no dup.
    await composer.press("ArrowUp");
    await expect(composer).toHaveValue("second queued");
    await composer.fill("second queued edited");
    await composer.press("Enter");
    await expect(page.getByRole("button", { name: /^second queued edited$/ })).toBeVisible({ timeout: 5000 });
    await expect(page.getByRole("button", { name: /^second queued$/ })).toHaveCount(0);
    await expect(page.getByRole("button", { name: /^first queued$/ })).toBeVisible();
    await expect(page.getByText(/Editing queued message/)).toHaveCount(0);
  });
});
