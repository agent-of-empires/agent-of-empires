// Accessory feature pings: each action posts its registry name to /api/telemetry/feature-use.
// Vitest stubs `reportFeatureUse` globally, so this is where the sent names are checked.

import { test, expect, pressShortcut } from "./helpers/mockedTest";
import { sessionResponse } from "./helpers/sessions";
import { mockStaticApis } from "./helpers/apiMocks";
import { Page } from "@playwright/test";

async function recordFeatureUses(page: Page): Promise<string[]> {
  const sent: string[] = [];
  await page.route("**/api/telemetry/feature-use", (r) => {
    const body = JSON.parse(r.request().postData() || "{}") as { feature?: string };
    if (body.feature) sent.push(body.feature);
    return r.fulfill({ status: 204 });
  });
  return sent;
}

test.describe("Feature-use telemetry", () => {
  test("view menu, attention shortcut and cheat code post their registry names", async ({ page }) => {
    await mockStaticApis(page);
    await page.route("**/api/sidebar-view", (r) => r.fulfill({ json: { group_by: "project", sort: "name" } }));
    await page.route(/\/api\/sessions(\?.*)?$/, (r) =>
      r.fulfill({
        headers: { "aoe-runtime-epoch": "mock-runtime", "aoe-runtime-revision": "1" },
        json: {
          sessions: [
            sessionResponse({
              id: "s-1",
              title: "one",
              project_path: "/tmp/repo",
              branch: "feature/one",
              created_at: "2025-01-01T00:00:00Z",
            }),
          ],
          workspace_ordering: ["/tmp/repo::feature/one"],
        },
      }),
    );
    const sent = await recordFeatureUses(page);
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/");

    const toggle = page.locator("[data-testid='sidebar-view-toggle']");
    await toggle.click();
    await page.locator("[data-testid='sidebar-view-sort-name']").click();
    await expect(toggle).toHaveAttribute("data-sort", "name");
    await expect.poll(() => sent).toContain("sort_az");

    await page.locator("body").press("a");
    await expect.poll(() => sent).toContain("attention_jump");

    await pressShortcut(page, "ControlOrMeta+k");
    const search = page.getByPlaceholder("Search actions, sessions, settings…");
    await search.fill("wololo");
    await expect(page.getByText(/converts to your cause/i)).toBeVisible();
    await expect.poll(() => sent).toContain("cheat_code");
  });
});
