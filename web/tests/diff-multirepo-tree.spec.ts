import { test, expect } from "./helpers/mockedTest";
import { Page } from "@playwright/test";
import { clickSidebarSession } from "./helpers/sidebar";

// Multi-repo workspaces should let the user fold subfolders inside
// each per-repo group, just like single-repo tree view. Asserts the
// view-mode toggle is visible and that tree mode produces foldable
// dir rows nested under each repo header, with per-repo state
// isolation.

async function setupMultiRepoSession(page: Page) {
  await page.route("**/api/login/status", (r) =>
    r.fulfill({ json: { required: false, authenticated: true } }),
  );
  await page.route("**/api/sessions", (r) => {
    if (r.request().method() === "POST") return r.fulfill({ status: 400 });
    return r.fulfill({
      json: {
        sessions: [
          {
            id: "multi-repo",
            title: "multi-repo",
            project_path: "/tmp/multi",
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
            workspace_repos: [
              { name: "repo-a", source_path: "/tmp/multi/repo-a" },
              { name: "repo-b", source_path: "/tmp/multi/repo-b" },
            ],
          },
        ],
        workspace_ordering: [],
      },
    });
  });
  await page.route("**/api/sessions/*/ensure", (r) =>
    r.fulfill({ json: { ok: true } }),
  );
  await page.route("**/api/sessions/*/terminal", (r) =>
    r.fulfill({ status: 200, body: "" }),
  );
  for (const path of [
    "settings",
    "themes",
    "agents",
    "profiles",
    "groups",
    "devices",
    "docker/status",
    "about",
  ]) {
    await page.route(`**/api/${path}`, (r) =>
      r.fulfill({ json: path === "docker/status" ? {} : [] }),
    );
  }
  await page.routeWebSocket(/\/sessions\/.*\/(ws|container-ws)$/, () => {});
  await page.route("**/api/sessions/*/diff/files", (r) =>
    r.fulfill({
      json: {
        files: [
          {
            path: "src/api/server.ts",
            old_path: null,
            status: "modified",
            additions: 5,
            deletions: 1,
            repo_name: "repo-a",
          },
          {
            path: "src/api/routes.ts",
            old_path: null,
            status: "added",
            additions: 12,
            deletions: 0,
            repo_name: "repo-a",
          },
          {
            path: "src/web/index.tsx",
            old_path: null,
            status: "modified",
            additions: 3,
            deletions: 3,
            repo_name: "repo-b",
          },
          {
            path: "src/web/utils/format.ts",
            old_path: null,
            status: "added",
            additions: 8,
            deletions: 0,
            repo_name: "repo-b",
          },
        ],
        per_repo_bases: [
          { repo_name: "repo-a", base_branch: "main" },
          { repo_name: "repo-b", base_branch: "main" },
        ],
        warning: null,
      },
    }),
  );
}

test.use({ viewport: { width: 1280, height: 720 } });

test.describe("Diff multi-repo subfolder folding", () => {
  test("the view-mode toggle is shown in multi-repo mode", async ({ page }) => {
    await setupMultiRepoSession(page);
    await page.goto("/");
    await expect(page.locator("header")).toBeVisible();
    await clickSidebarSession(page, "multi-repo");
    await expect(
      page.getByText("2 repos", { exact: true }).first(),
    ).toBeVisible({
      timeout: 10000,
    });
    // Toggle title flips based on the current mode. Match either so the
    // assertion stays robust against the desktop default.
    await expect(
      page
        .locator(
          'button[title="Switch to tree view"], button[title="Switch to flat list"]',
        )
        .first(),
    ).toBeVisible();
  });

  test("tree mode renders foldable dir rows inside each repo group", async ({
    page,
  }) => {
    await setupMultiRepoSession(page);
    await page.goto("/");
    await clickSidebarSession(page, "multi-repo");
    await expect(
      page.getByText("2 repos", { exact: true }).first(),
    ).toBeVisible({
      timeout: 10000,
    });
    // Force tree mode regardless of viewport default: if the toggle
    // title is "Switch to tree view", we're in flat mode and need to
    // click it; otherwise we're already in tree.
    const toFlat = page.locator('button[title="Switch to flat list"]').first();
    const toTree = page.locator('button[title="Switch to tree view"]').first();
    if (await toTree.isVisible().catch(() => false)) {
      await toTree.click();
    }
    await expect(toFlat).toBeVisible();
    // Two `web` dir rows would only appear if we re-rendered the same
    // tree twice; tree mode collapses repo-b's `src → web → utils →`
    // chain so `web` shows once.
    const webDir = page.getByRole("button", { name: /^web/ });
    await expect(webDir.first()).toBeVisible();
    await expect(page.getByText("format.ts").first()).toBeVisible();
    await webDir.first().click();
    await expect(page.getByText("format.ts").first()).toBeHidden();
    // Folding `web/` in repo-b must not collapse `api/` in repo-a.
    // The namespaced collapsed-dirs key keeps each repo independent.
    await expect(page.getByText("routes.ts").first()).toBeVisible();
  });
});
