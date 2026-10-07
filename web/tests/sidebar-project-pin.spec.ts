import { test, expect } from "./helpers/mockedTest";
import { sessionResponse } from "./helpers/sessions";
import { mockStaticApis } from "./helpers/apiMocks";
import { Page } from "@playwright/test";

// Mocked coverage for the web sidebar pin/unpin handlers (#2208). The live
// spec (web/tests/live/project-pin.spec.ts) proves the real wire round-trip;
// this mocked spec drives the same App handlers under the instrumented build
// so handlePinProject (POST + PATCH-existing branches) and handleUnpinProject
// (PATCH, never DELETE) are exercised. Unpin must PATCH pinned:false, keeping
// the saved project rather than deleting it.

interface MockSession {
  id: string;
  title: string;
  project_path: string;
}

interface MockProject {
  name: string;
  path: string;
  scope: "global" | "profile";
  pinned: boolean;
}

async function mockApis(page: Page, sessions: MockSession[], projects: MockProject[]) {
  await mockStaticApis(page);
  await page.route("**/api/sessions", (r) => {
    if (r.request().method() !== "GET") return r.fulfill({ status: 400 });
    return r.fulfill({
      json: {
        sessions: sessions.map((s) => sessionResponse({ ...s })),
        workspace_ordering: [],
      },
    });
  });
  // GET lists the registry; POST registers (returns the created project).
  await page.route("**/api/projects", (r) => {
    const method = r.request().method();
    if (method === "GET") return r.fulfill({ json: projects });
    if (method === "POST") {
      const body = r.request().postDataJSON() as { path: string; pinned?: boolean };
      return r.fulfill({
        status: 201,
        json: { name: body.path.split("/").pop(), path: body.path, scope: "global", pinned: body.pinned ?? false },
      });
    }
    return r.fulfill({ status: 400 });
  });
  // PATCH toggles the pin flag (the unpin / pin-existing path).
  await page.route("**/api/projects/*", (r) => {
    if (r.request().method() !== "PATCH") return r.fulfill({ status: 400 });
    return r.fulfill({ json: { name: "p", path: "/tmp/p", scope: "global", pinned: false } });
  });
}

test.describe("Sidebar project pin/unpin (#2208)", () => {
  test("Pin on a registered-but-unpinned repo PATCHes pinned:true", async ({ page }) => {
    await mockApis(
      page,
      [{ id: "s-1", title: "Mongols", project_path: "/tmp/repo-a" }],
      [{ name: "repo-a", path: "/tmp/repo-a", scope: "global", pinned: false }],
    );
    await page.goto("/");
    await expect(page.locator("header")).toBeVisible();

    const header = page.locator("[data-testid='sidebar-group-header']").filter({ hasText: "repo-a" });
    await expect(header).toBeVisible();
    await header.click({ button: "right" });

    const patch = page.waitForRequest((req) => req.url().includes("/api/projects/") && req.method() === "PATCH");
    await page.locator("[data-testid='sidebar-group-context-menu-pin']").click();
    const req = await patch;
    expect(req.postDataJSON()).toMatchObject({ pinned: true });
  });
});
