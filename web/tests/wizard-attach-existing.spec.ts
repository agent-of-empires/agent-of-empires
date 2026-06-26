import { test, expect } from "./helpers/mockedTest";
import { Page } from "@playwright/test";
import { openWizard, selectProject, expandMoreOptions, wizard } from "./helpers/wizard";

// Wizard "Attach to existing branch" toggle (#969) on the single-screen
// wizard (#2210). Mirrors the TUI's `Attach to existing branch:` checkbox:
// when on, the request body sends `create_new_branch: false` (and the
// Base branch section hides since it's only honored for new-branch
// creates). The toggle lives under the single "More options" fold.

async function mockApis(page: Page) {
  await page.route("**/api/login/status", (r) => r.fulfill({ json: { required: false, authenticated: true } }));
  for (const path of ["settings", "themes", "profiles", "groups", "devices", "about", "system/update-status"]) {
    await page.route(`**/api/${path}`, (r) =>
      r.fulfill({
        // worktree.enabled drives the wizard's "Create a worktree" default (#2423);
        // the attach-existing toggle only renders while worktree is on.
        json:
          path === "settings"
            ? { worktree: { enabled: true } }
            : path === "about" || path === "system/update-status"
              ? {}
              : [],
      }),
    );
  }
  await page.route("**/api/recent-projects", (r) => r.fulfill({ json: { projects: [] } }));
  await page.route("**/api/projects", (r) => r.fulfill({ json: [] }));
  await page.route("**/api/docker/status", (r) => r.fulfill({ json: { available: false, runtime: null } }));
  await page.route("**/api/agents", (r) =>
    r.fulfill({
      json: [
        {
          name: "claude",
          binary: "claude",
          host_only: false,
          installed: true,
          install_hint: "",
        },
      ],
    }),
  );
  await page.route("**/api/sessions", (r) => {
    if (r.request().method() === "GET") {
      return r.fulfill({
        json: {
          sessions: [
            {
              id: "seed-session",
              title: "seed",
              project_path: "/tmp/example",
              group_path: "/tmp",
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
            },
          ],
          workspace_ordering: [],
        },
      });
    }
    return r.fulfill({ json: { session: { id: "new-session" } } });
  });
}

async function openWithProject(page: Page) {
  await openWizard(page);
  await selectProject(page, "/tmp/example");
}

function attachToggle(page: Page) {
  return wizard(page).getByRole("switch", { name: /Attach to existing branch/ });
}

test.describe("Wizard attach-existing toggle (#969)", () => {
  test("toggle is off by default; Base branch section visible", async ({ page }) => {
    await mockApis(page);
    await page.setViewportSize({ width: 1280, height: 900 });
    await page.goto("/");
    await openWithProject(page);
    await expandMoreOptions(page);
    await expect(attachToggle(page)).toBeVisible();
    await expect(attachToggle(page)).toHaveAttribute("aria-checked", "false");
    // The base-branch picker (only meaningful for new-branch creates) is
    // its own "Base branch" disclosure under More options.
    await expect(wizard(page).getByRole("button", { name: "Base branch" })).toBeVisible();
  });

  test("turning attach on hides the Base branch section", async ({ page }) => {
    await mockApis(page);
    await page.setViewportSize({ width: 1280, height: 900 });
    await page.goto("/");
    await openWithProject(page);
    await expandMoreOptions(page);
    await attachToggle(page).click();
    await expect(attachToggle(page)).toHaveAttribute("aria-checked", "true");
    await expect(wizard(page).getByRole("button", { name: "Base branch" })).toHaveCount(0);
  });

  test("submit with attach off sends create_new_branch=true", async ({ page }) => {
    await mockApis(page);
    let captured: { create_new_branch?: boolean; base_branch?: string } | null = null;
    await page.route("**/api/sessions", (r) => {
      if (r.request().method() === "POST") {
        captured = JSON.parse(r.request().postData() || "{}");
        return r.fulfill({ json: { session: { id: "new-session" } } });
      }
      return r.fulfill({
        json: {
          sessions: [
            {
              id: "seed-session",
              title: "seed",
              project_path: "/tmp/example",
              group_path: "/tmp",
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
            },
          ],
          workspace_ordering: [],
        },
      });
    });
    await page.setViewportSize({ width: 1280, height: 900 });
    await page.goto("/");
    await openWithProject(page);
    await expandMoreOptions(page);
    await wizard(page).getByPlaceholder("Uses session title if empty").fill("feat/new");
    await wizard(page)
      .getByRole("button", { name: /Launch session/ })
      .click();
    await expect.poll(() => captured?.create_new_branch).toBe(true);
  });

  test("submit with attach on sends create_new_branch=false and no base_branch", async ({ page }) => {
    await mockApis(page);
    let captured: { create_new_branch?: boolean; base_branch?: string } | null = null;
    await page.route("**/api/sessions", (r) => {
      if (r.request().method() === "POST") {
        captured = JSON.parse(r.request().postData() || "{}");
        return r.fulfill({ json: { session: { id: "new-session" } } });
      }
      return r.fulfill({
        json: {
          sessions: [
            {
              id: "seed-session",
              title: "seed",
              project_path: "/tmp/example",
              group_path: "/tmp",
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
            },
          ],
          workspace_ordering: [],
        },
      });
    });
    await page.setViewportSize({ width: 1280, height: 900 });
    await page.goto("/");
    await openWithProject(page);
    await expandMoreOptions(page);
    await wizard(page).getByPlaceholder("Uses session title if empty").fill("feat/existing");
    await attachToggle(page).click();
    await wizard(page)
      .getByRole("button", { name: /Launch session/ })
      .click();
    await expect.poll(() => captured?.create_new_branch).toBe(false);
    expect(captured?.base_branch).toBeUndefined();
  });
});
