import { test, expect } from "./helpers/mockedTest";
import { Page } from "@playwright/test";
import { openWizard, selectProject, expandMoreOptions, wizard } from "./helpers/wizard";

// Wizard session controls (#1219) on the single-screen wizard (#2210).
// Covers title, worktree toggle gating of the branch input + Base branch
// section, and the group field. All of these now live under the single
// "More options" fold. The "Attach to existing branch" toggle has its own
// dedicated spec (wizard-attach-existing.spec.ts, #969); the base-branch
// picker has wizard-base-branch.spec.ts (#948). Don't duplicate those here.

async function mockApis(page: Page) {
  await page.route("**/api/login/status", (r) => r.fulfill({ json: { required: false, authenticated: true } }));
  for (const path of ["settings", "themes", "profiles", "groups", "devices", "about", "system/update-status"]) {
    await page.route(`**/api/${path}`, (r) =>
      r.fulfill({
        // worktree.enabled drives the wizard's "Create a worktree" default (#2423);
        // these worktree-flow specs assume it is on.
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

test.describe("Wizard session step (#1219)", () => {
  test("title input is empty by default and updates as the user types", async ({ page }) => {
    await mockApis(page);
    await page.setViewportSize({ width: 1280, height: 900 });
    await page.goto("/");
    await openWithProject(page);
    // Title is an always-visible essential, not folded under More options.
    const titleInput = wizard(page).getByPlaceholder("Auto-generated if empty");
    await expect(titleInput).toHaveValue("");
    await titleInput.fill("my-feature");
    await expect(titleInput).toHaveValue("my-feature");
  });

  test("worktree toggle follows worktree.enabled (on) and gates the branch input + Base branch section", async ({
    page,
  }) => {
    await mockApis(page);
    await page.setViewportSize({ width: 1280, height: 900 });
    await page.goto("/");
    await openWithProject(page);
    await expandMoreOptions(page);
    const w = wizard(page);
    const worktreeToggle = w.getByRole("switch", {
      name: /Create a worktree/,
    });
    await expect(worktreeToggle).toHaveAttribute("aria-checked", "true");
    // Branch input visible while worktree is on.
    await expect(w.getByPlaceholder("Uses session title if empty")).toBeVisible();
    await expect(w.getByRole("button", { name: "Base branch" })).toBeVisible();
    // Flip off: branch input + Base branch picker disappear. The "More
    // options" fold stays open.
    await worktreeToggle.click();
    await expect(worktreeToggle).toHaveAttribute("aria-checked", "false");
    await expect(w.getByPlaceholder("Uses session title if empty")).toHaveCount(0);
    await expect(w.getByRole("button", { name: "Base branch" })).toHaveCount(0);
    // Flip back on: the branch input re-mounts (ported from the live
    // wizard-worktree-toggle story).
    await worktreeToggle.click();
    await expect(worktreeToggle).toHaveAttribute("aria-checked", "true");
    await expect(w.getByPlaceholder("Uses session title if empty")).toBeVisible();
  });

  test("worktree toggle defaults off when worktree.enabled is false (#2423)", async ({ page }) => {
    await mockApis(page);
    // Override the settings stub: worktree.enabled false must default the
    // "Create a worktree" toggle off. The later route wins (LIFO).
    await page.route("**/api/settings", (r) => r.fulfill({ json: { worktree: { enabled: false } } }));
    await page.setViewportSize({ width: 1280, height: 900 });
    await page.goto("/");
    await openWithProject(page);
    await expandMoreOptions(page);
    const w = wizard(page);
    const worktreeToggle = w.getByRole("switch", { name: /Create a worktree/ });
    await expect(worktreeToggle).toHaveAttribute("aria-checked", "false");
    // Branch input + Base branch picker stay hidden while worktree is off.
    await expect(w.getByPlaceholder("Uses session title if empty")).toHaveCount(0);
    await expect(w.getByRole("button", { name: "Base branch" })).toHaveCount(0);
  });

  test("branch input mirrors the slugified title while not manually edited", async ({ page }) => {
    await mockApis(page);
    await page.setViewportSize({ width: 1280, height: 900 });
    await page.goto("/");
    await openWithProject(page);
    await expandMoreOptions(page);
    const w = wizard(page);
    await w.getByPlaceholder("Auto-generated if empty").fill("My Cool Feature");
    const branchInput = w.getByPlaceholder("Uses session title if empty");
    // SET_FIELD on title cascades into worktreeBranch via slugifyBranch().
    await expect(branchInput).toHaveValue("my-cool-feature");
  });

  test("submit sends worktree_enabled + create_new_branch with More options left closed", async ({ page }) => {
    await mockApis(page);
    let captured: {
      worktree_enabled?: boolean;
      worktree_branch?: string;
      create_new_branch?: boolean;
    } | null = null;
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
    // Fill the always-visible title and launch with More options never
    // opened. The worktree toggle defaults on, so the default new-worktree
    // behavior must still reach the request body.
    const w = wizard(page);
    await w.getByPlaceholder("Auto-generated if empty").fill("Cool Feature");
    await w.getByRole("button", { name: /Launch session/ }).click();
    await expect.poll(() => captured?.worktree_enabled).toBe(true);
    expect(captured?.worktree_branch).toBeUndefined();
    expect(captured?.create_new_branch).toBe(true);
  });

  test("group input propagates to the create-session POST body", async ({ page }) => {
    await mockApis(page);
    let captured: { group?: string } | null = null;
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
    const w = wizard(page);
    await w.getByPlaceholder("Optional, for organizing related sessions").fill("backend");
    await w.getByRole("button", { name: /Launch session/ }).click();
    await expect.poll(() => captured?.group).toBe("backend");
  });

  test("worktree / group controls hide behind collapsed More options by default", async ({ page }) => {
    await mockApis(page);
    await page.setViewportSize({ width: 1280, height: 900 });
    await page.goto("/");
    await openWithProject(page);
    const w = wizard(page);
    // More options is collapsed, so all worktree / group controls are folded
    // away. The title is an essential and stays visible.
    await expect(w.getByRole("button", { name: "More options" })).toHaveAttribute("aria-expanded", "false");
    await expect(w.getByPlaceholder("Auto-generated if empty")).toBeVisible();
    await expect(w.getByPlaceholder("Uses session title if empty")).toHaveCount(0);
    await expect(w.getByPlaceholder("Optional, for organizing related sessions")).toHaveCount(0);
    await expect(w.getByRole("button", { name: "Base branch" })).toHaveCount(0);
  });

  test("expanding More options reveals the worktree toggle, branch, attach toggle, and group", async ({ page }) => {
    await mockApis(page);
    await page.setViewportSize({ width: 1280, height: 900 });
    await page.goto("/");
    await openWithProject(page);
    await expandMoreOptions(page);
    const w = wizard(page);
    // Worktree toggle defaults on.
    await expect(w.getByRole("switch", { name: /Create a worktree/ })).toHaveAttribute("aria-checked", "true");
    await expect(w.getByPlaceholder("Uses session title if empty")).toBeVisible();
    await expect(w.getByRole("switch", { name: /Attach to existing branch/ })).toBeVisible();
    await expect(w.getByRole("button", { name: "Base branch" })).toBeVisible();
    // Group input is visible and editable.
    const groupInput = w.getByPlaceholder("Optional, for organizing related sessions");
    await expect(groupInput).toBeVisible();
    await groupInput.fill("backend");
    await expect(groupInput).toHaveValue("backend");
  });
});
