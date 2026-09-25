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
  await page.route("**/api/projects*", (r) => {
    const request = r.request();
    if (request.method() === "GET") {
      if (new URL(request.url()).searchParams.get("profile") !== "default") return r.fulfill({ status: 400 });
      return r.fulfill({ json: projects });
    }
    if (request.method() === "POST") {
      const body = request.postDataJSON() as { path: string; pinned?: boolean; profile: string; scope: string };
      if (body.profile !== "default" || body.scope !== "global") return r.fulfill({ status: 400 });
      if (projects.some((project) => project.path === body.path)) return r.fulfill({ status: 409 });
      const project: MockProject = {
        name: body.path.split("/").pop()!,
        path: body.path,
        scope: "global",
        pinned: body.pinned ?? false,
      };
      projects.push(project);
      return r.fulfill({ status: 201, json: project });
    }
    return r.fulfill({ status: 400 });
  });
  await page.route("**/api/projects/*", (r) => {
    if (r.request().method() !== "PATCH") return r.fulfill({ status: 400 });
    const url = new URL(r.request().url());
    const path = decodeURIComponent(url.pathname.slice("/api/projects/".length));
    const project = projects.find(
      (project) => project.path === path && project.scope === url.searchParams.get("scope"),
    );
    if (!project) return r.fulfill({ status: 404 });
    project.pinned = r.request().postDataJSON().pinned;
    return r.fulfill({ json: project });
  });
  for (const path of ["settings", "themes", "agents", "profiles", "groups", "devices", "docker/status", "about"]) {
    await page.route(`**/api/${path}`, (r) =>
      r.fulfill({
        json: path === "profiles" ? [{ name: "default", is_default: true }] : path === "docker/status" ? {} : [],
      }),
    );
  }
}

test.describe("Sidebar project pin/unpin", () => {
  for (const alreadySaved of [false, true]) {
    test(`Pin keeps a ${alreadySaved ? "saved" : "new"} project visible after its last session disappears`, async ({
      page,
    }) => {
      const sessions = [{ id: "s-1", title: "Mongols", project_path: "/tmp/repo-a" }];
      await mockApis(
        page,
        sessions,
        alreadySaved ? [{ name: "repo-a", path: "/tmp/repo-a", scope: "global", pinned: false }] : [],
      );
      await page.goto("/");
      const header = page.getByTestId("sidebar-group-header").filter({ hasText: "repo-a" });
      await expect(header).toBeVisible();
      await header.click({ button: "right" });
      const response = page.waitForResponse(
        (response) => response.url().includes("/api/projects") && response.request().method() !== "GET",
      );
      await page.getByTestId("sidebar-group-context-menu-pin").click();
      await response;
      sessions.splice(0);
      await page.reload();
      await expect(header).toBeVisible();
    });
  }

  test("Unpin removes the sessionless header but preserves its saved project", async ({ page }) => {
    await mockApis(page, [], [{ name: "repo-b", path: "/tmp/repo-b", scope: "global", pinned: true }]);
    await page.goto("/");
    const header = page.getByTestId("sidebar-group-header").filter({ hasText: "repo-b" });
    await expect(header).toBeVisible();
    await header.click({ button: "right" });
    await page.getByTestId("sidebar-group-context-menu-unpin").click();
    await expect(header).toBeHidden();
    const toggle = page.getByTestId("sidebar-projects-toggle");
    if ((await toggle.getAttribute("aria-expanded")) === "false") await toggle.click();
    await expect(page.getByTestId("sidebar-project-row").filter({ hasText: "repo-b" })).toBeVisible();
  });
});
