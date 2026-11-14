// #5636: an Error Session says what AoE recorded. On a phone the row has no tooltip, so tapping it opens the
// Session, whose page carries the full error; opening it must not restart it (#5055). On a desktop the glyph's
// tooltip shows the first line.

import type { Page } from "@playwright/test";
import { test, expect } from "./helpers/mockedTest";
import { mockStaticApis } from "./helpers/apiMocks";
import { sessionResponse } from "./helpers/sessions";
import { DESKTOP } from "./helpers/viewports";

const ERROR = "resume failed for sid 55fd0b02; preserved for explicit retry\nsecond line of detail";

async function mockErrorSession(page: Page, lastError: string | null) {
  const state = { status: "Error", last_error: lastError, ensures: 0, restarts: 0 };
  await mockStaticApis(page);
  await page.route(/\/api\/sessions(\?.*)?$/, (r) =>
    r.fulfill({
      headers: { "aoe-runtime-epoch": "mock-runtime", "aoe-runtime-revision": String(state.restarts + 1) },
      json: {
        sessions: [
          sessionResponse({
            id: "groceries",
            title: "Groceries",
            project_path: "/tmp/groceries",
            status: state.status,
            last_error: state.last_error,
          }),
        ],
        workspace_ordering: [],
      },
    }),
  );
  await page.route("**/api/sessions/*/ensure", (r) => {
    state.ensures += 1;
    return r.fulfill({ json: { ok: true } });
  });
  await page.route("**/api/sessions/*/restart", (r) => {
    state.restarts += 1;
    state.status = "Idle";
    state.last_error = null;
    return r.fulfill({ json: { ok: true } });
  });
  await page.routeWebSocket(/\/sessions\/.*\/(?:live-ws|ws|acp-ws)(?:\?.*)?$/, () => {});
  return state;
}

test.describe("An Error Session's recorded error (#5636)", () => {
  test.describe("on a phone", () => {
    test.use({ viewport: { width: 375, height: 812 }, hasTouch: true });

    test("tapping the row shows the full error; Copy and Restart work, and opening didn't restart it", async ({
      page,
      context,
    }) => {
      await context.grantPermissions(["clipboard-read", "clipboard-write"]);
      const state = await mockErrorSession(page, ERROR);
      await page.goto("/");
      await page.getByRole("button", { name: "Toggle sidebar" }).click();
      await page.getByTestId("sidebar-session-row").tap();

      const panel = page.getByTestId("session-error-panel");
      await expect(panel.getByRole("heading")).toHaveText("Groceries reported an error");
      await expect(page.getByTestId("session-error-text")).toHaveText(ERROR);
      expect(state.ensures).toBe(0);

      await page.getByTestId("session-error-copy").tap();
      await expect(page.getByTestId("session-error-copy")).toHaveText("Copied");
      expect(await page.evaluate(() => navigator.clipboard.readText())).toBe(ERROR);

      await page.getByTestId("session-error-restart").tap();
      await expect(panel).toBeHidden();
      expect(state.restarts).toBe(1);
      await expect.poll(() => state.ensures).toBe(1);
    });

    test("with nothing recorded, the page says so and still offers Restart", async ({ page }) => {
      const state = await mockErrorSession(page, null);
      await page.goto("/session/groceries");
      const panel = page.getByTestId("session-error-panel");
      await expect(panel).toContainText("No details were recorded. Restart to try again.");
      await expect(page.getByTestId("session-error-text")).toHaveCount(0);
      await expect(page.getByTestId("session-error-copy")).toHaveCount(0);
      await expect(page.getByTestId("session-error-restart")).toBeVisible();
      expect(state.ensures).toBe(0);
    });
  });

  test.describe("on a desktop", () => {
    test.use(DESKTOP);

    test("hovering the glyph shows the first line of the error", async ({ page }) => {
      await mockErrorSession(page, ERROR);
      await page.goto("/");
      await page.getByTestId("sidebar-status-glyph").hover();
      const tooltip = page.getByRole("tooltip");
      await expect(tooltip).toContainText("Error");
      await expect(tooltip).toContainText("resume failed for sid 55fd0b02; preserved for explicit retry");
      await expect(tooltip).not.toContainText("second line of detail");
    });
  });
});
