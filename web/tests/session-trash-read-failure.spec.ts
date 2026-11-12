import { test, expect } from "./helpers/mockedTest";
import { mockSessionShellApis, mockStaticApis } from "./helpers/apiMocks";
import { sessionResponse } from "./helpers/sessions";

test.use({ viewport: { width: 1280, height: 720 } });

// A direct link to a trashed session while the scoped Trash read fails: the pane must say so and offer Retry,
// not sit on the skeleton or claim the session is gone.
test.describe("Trash read failure on a retained session link", () => {
  test("shows an error with Retry, then recovers once the read lands", async ({ page }) => {
    const trashedAt = new Date().toISOString();
    const trashedSession = sessionResponse({
      id: "sess-trashed",
      title: "story-trashed",
      project_path: "/tmp/story",
      group_path: "/tmp",
      status: "Stopped",
      trashed_at: trashedAt,
    });
    const headers = { "aoe-runtime-epoch": "mock-runtime", "aoe-runtime-revision": "1" };
    let trashFails = true;

    await mockStaticApis(page);
    await mockSessionShellApis(page);
    await page.route(/\/api\/sessions\?state=untrashed$/, (r) =>
      r.fulfill({ headers, json: { sessions: [], workspace_ordering: [], trashed_count: 1 } }),
    );
    await page.route(/\/api\/sessions\?state=trashed$/, (r) =>
      trashFails
        ? r.fulfill({ status: 500, body: "boom" })
        : r.fulfill({ headers, json: { sessions: [trashedSession], workspace_ordering: [], trashed_count: 1 } }),
    );

    await page.goto("/session/sess-trashed");

    const failed = page.getByTestId("trash-read-failed");
    await expect(failed).toBeVisible({ timeout: 10_000 });
    await expect(page.getByTestId("focus-session-gone")).toHaveCount(0);

    trashFails = false;
    await failed.getByRole("button", { name: "Retry" }).click();
    await expect(failed).toHaveCount(0, { timeout: 10_000 });
  });
});
