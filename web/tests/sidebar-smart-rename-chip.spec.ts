import { test, expect } from "./helpers/mockedTest";
import { Page } from "@playwright/test";

// Renders the smart-rename sidebar chips through the real bundle so their JSX
// lines get per-line istanbul coverage (vitest v8 attributes the whole
// `{cond && <span/>}` to the outer expression, leaving the span body lines
// uncounted). The chip only keys off `smart_rename`, so a plain session row is
// enough to exercise both the `pending` and `running` branches.

interface MockSession {
  id: string;
  title: string;
  project_path: string;
  smart_rename: "inactive" | "pending" | "running";
}

async function mockApis(page: Page, sessions: MockSession[]) {
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
          smart_rename: s.smart_rename,
        })),
        workspace_ordering: [],
      },
    });
  });
  for (const path of ["settings", "themes", "agents", "profiles", "groups", "devices", "docker/status", "about"]) {
    await page.route(`**/api/${path}`, (r) => r.fulfill({ json: path === "docker/status" ? {} : [] }));
  }
}

test.describe("Sidebar smart-rename chips", () => {
  test("renders the Auto-name (pending) and Naming (running) chips", async ({ page }) => {
    await mockApis(page, [
      { id: "sess-pending", title: "Vikings", project_path: "/tmp/p-pending", smart_rename: "pending" },
      { id: "sess-running", title: "Franks", project_path: "/tmp/p-running", smart_rename: "running" },
      { id: "sess-inactive", title: "My session", project_path: "/tmp/p-inactive", smart_rename: "inactive" },
    ]);
    await page.goto("/");

    await expect(page.getByLabel("Will auto-name")).toBeVisible();
    await expect(page.getByLabel("Naming")).toBeVisible();
    // The inactive session shows neither chip; exactly one of each is present.
    await expect(page.getByLabel("Will auto-name")).toHaveCount(1);
    await expect(page.getByLabel("Naming")).toHaveCount(1);
  });
});
