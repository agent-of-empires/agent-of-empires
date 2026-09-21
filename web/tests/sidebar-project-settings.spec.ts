import { test, expect } from "./helpers/mockedTest";
import { Page } from "@playwright/test";

// Mocked coverage for the active-project group header's "Project settings"
// action: an active project (one with live sessions) previously had no path
// to the base-branch/worktree/smart-rename override editor at all, which
// only lived in the sessionless Projects section (see ProjectsSection.tsx's
// "Project settings" row). This proves the group header offers the same
// editor, registering the repo on the fly when it was never explicitly saved.

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
  default_base_branch?: string;
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
      const body = r.request().postDataJSON() as { path: string };
      return r.fulfill({
        status: 201,
        json: { name: body.path.split("/").pop(), path: body.path, scope: "global", pinned: false },
      });
    }
    return r.fulfill({ status: 400 });
  });
  for (const path of ["settings", "themes", "agents", "profiles", "groups", "devices", "docker/status", "about"]) {
    await page.route(`**/api/${path}`, (r) => r.fulfill({ json: path === "docker/status" ? {} : [] }));
  }
}

test.describe("Sidebar active-project settings (#4036)", () => {
  test("Project settings on an unregistered populated repo registers it, then opens the editor", async ({ page }) => {
    await mockApis(page, [{ id: "s-1", title: "Mongols", project_path: "/tmp/repo-a" }], []);
    await page.goto("/");
    await expect(page.locator("header")).toBeVisible();

    const header = page.locator("[data-testid='sidebar-group-header']").filter({ hasText: "repo-a" });
    await expect(header).toBeVisible();
    await header.click({ button: "right" });

    const post = page.waitForRequest((req) => req.url().endsWith("/api/projects") && req.method() === "POST");
    await page.locator("[data-testid='sidebar-group-context-menu-settings']").click();
    const req = await post;
    expect(req.postDataJSON()).toMatchObject({ path: "/tmp/repo-a", scope: "global" });

    await expect(page.getByTestId("project-form-modal")).toBeVisible();
    await expect(page.getByText("Edit project 'repo-a'")).toBeVisible();
  });

  test("Project settings on an already-registered repo opens the editor directly, no re-registration", async ({
    page,
  }) => {
    await mockApis(
      page,
      [{ id: "s-1", title: "Mongols", project_path: "/tmp/repo-a" }],
      [{ name: "repo-a", path: "/tmp/repo-a", scope: "global", pinned: false, default_base_branch: "develop" }],
    );
    await page.goto("/");
    await expect(page.locator("header")).toBeVisible();

    const header = page.locator("[data-testid='sidebar-group-header']").filter({ hasText: "repo-a" });
    await expect(header).toBeVisible();
    await header.click({ button: "right" });

    let posted = false;
    page.on("request", (req) => {
      if (req.url().endsWith("/api/projects") && req.method() === "POST") posted = true;
    });
    await page.locator("[data-testid='sidebar-group-context-menu-settings']").click();

    await expect(page.getByTestId("project-form-modal")).toBeVisible();
    await expect(page.getByText("Edit project 'repo-a'")).toBeVisible();
    expect(posted).toBe(false);
  });
});
