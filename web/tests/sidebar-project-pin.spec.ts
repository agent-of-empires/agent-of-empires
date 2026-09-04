import { test, expect } from "./helpers/mockedTest";
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
  await page.route("**/api/login/status", (r) => r.fulfill({ json: { required: false, authenticated: true } }));
  await page.route("**/api/sessions", (r) => {
    if (r.request().method() !== "GET") return r.fulfill({ status: 400 });
    return r.fulfill({
      json: {
        sessions: sessions.map((s) => ({
          id: s.id,
          title: s.title,
          project_path: s.project_path,
          group_path: s.project_path,
          tool: "claude",
          status: "Idle",
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
        })),
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
  for (const path of ["settings", "themes", "agents", "profiles", "groups", "devices", "docker/status", "about"]) {
    await page.route(`**/api/${path}`, (r) => r.fulfill({ json: path === "docker/status" ? {} : [] }));
  }
}

test.describe("Sidebar project pin/unpin (#2208)", () => {
  test("Pin on an unregistered populated repo POSTs pinned:true", async ({ page }) => {
    await mockApis(page, [{ id: "s-1", title: "Mongols", project_path: "/tmp/repo-a" }], []);
    await page.goto("/");
    await expect(page.locator("header")).toBeVisible();

    const header = page.locator("[data-testid='sidebar-group-header']").filter({ hasText: "repo-a" });
    await expect(header).toBeVisible();
    await header.click({ button: "right" });

    const post = page.waitForRequest((req) => req.url().endsWith("/api/projects") && req.method() === "POST");
    await page.locator("[data-testid='sidebar-group-context-menu-pin']").click();
    const req = await post;
    expect(req.postDataJSON()).toMatchObject({ path: "/tmp/repo-a", scope: "global", pinned: true });
  });

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

  test("Unpin a pinned-empty project PATCHes pinned:false (not DELETE)", async ({ page }) => {
    await mockApis(page, [], [{ name: "repo-b", path: "/tmp/repo-b", scope: "global", pinned: true }]);
    await page.goto("/");
    await expect(page.locator("header")).toBeVisible();

    const header = page.locator("[data-testid='sidebar-group-header']").filter({ hasText: "repo-b" });
    await expect(header).toBeVisible();
    await header.click({ button: "right" });

    const patch = page.waitForRequest((req) => req.url().includes("/api/projects/") && req.method() === "PATCH");
    await page.locator("[data-testid='sidebar-group-context-menu-unpin']").click();
    const req = await patch;
    expect(req.postDataJSON()).toMatchObject({ pinned: false });
    expect(req.method()).not.toBe("DELETE");
  });
});
