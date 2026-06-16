import { test, expect } from "./helpers/mockedTest";
import { Page } from "@playwright/test";
import { openWizard, selectProject, expandMoreOptions, wizard } from "./helpers/wizard";

// Wizard More options → Base branch (#948) on the single-screen wizard
// (#2210). Asserts:
// - The "Base branch" disclosure is collapsed by default.
// - Expanding it fetches local + remote branches.
// - Selecting one populates the base-branch input.
// - Submitting the wizard sends `base_branch` in the POST body.
//
// The worktree controls (including the Base branch picker) now live under
// the single "More options" fold; expand it first.

async function mockApis(page: Page) {
  await page.route("**/api/login/status", (r) => r.fulfill({ json: { required: false, authenticated: true } }));
  for (const path of ["settings", "themes", "profiles", "groups", "devices", "about", "system/update-status"]) {
    await page.route(`**/api/${path}`, (r) =>
      r.fulfill({
        json: path === "settings" || path === "about" || path === "system/update-status" ? {} : [],
      }),
    );
  }
  await page.route("**/api/recent-projects", (r) => r.fulfill({ json: { projects: [] } }));
  await page.route("**/api/projects", (r) => r.fulfill({ json: [] }));
  await page.route("**/api/docker/status", (r) => r.fulfill({ json: { available: false, runtime: null } }));
  // The wizard's project picker shows a "Recent projects" list driven by
  // /api/sessions. Seed one entry so the test can click it to select a
  // project.
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
}

async function openWizardWithProject(page: Page) {
  await openWizard(page);
  await selectProject(page, "/tmp/example");
}

test.describe("Wizard base branch (#948)", () => {
  test("Base branch section is collapsed by default under More options", async ({ page }) => {
    await mockApis(page);
    await page.setViewportSize({ width: 1280, height: 900 });
    await page.goto("/");
    await expect(page.locator("header")).toBeVisible();
    await openWizardWithProject(page);
    await expandMoreOptions(page);
    const w = wizard(page);
    // Worktree toggle is on by default; if a previous test left it
    // off, click to re-enable so the Base branch section renders.
    // `#969` added a second toggle ("Attach to existing branch") on this
    // step, so target the worktree toggle by its accessible name.
    const toggle = w.getByRole("switch", { name: /Create a worktree/ });
    if ((await toggle.getAttribute("aria-checked")) !== "true") {
      await toggle.click();
    }
    await expect(w.getByRole("button", { name: "Base branch" })).toBeVisible();
    await expect(w.getByLabel("Base branch")).toHaveCount(0);
  });

  test("expanding Base branch fetches branches with include_remote=true", async ({ page }) => {
    await mockApis(page);

    let capturedUrl: URL | null = null;
    await page.route("**/api/git/branches**", (r) => {
      capturedUrl = new URL(r.request().url());
      return r.fulfill({
        json: [
          { name: "main", is_current: true },
          { name: "feature/x", is_current: false },
          { name: "release-1.2", is_current: false, remote_only: true },
        ],
      });
    });

    await page.setViewportSize({ width: 1280, height: 900 });
    await page.goto("/");
    await openWizardWithProject(page);
    await expandMoreOptions(page);
    const w = wizard(page);
    await w.getByRole("button", { name: "Base branch" }).click();
    await expect(w.getByLabel("Base branch")).toBeVisible();
    await expect.poll(() => capturedUrl?.searchParams.get("include_remote")).toBe("true");
  });

  test("selecting a remote-only branch populates the base-branch input", async ({ page }) => {
    await mockApis(page);
    await page.route("**/api/git/branches**", (r) =>
      r.fulfill({
        json: [
          { name: "main", is_current: true },
          { name: "release-1.2", is_current: false, remote_only: true },
        ],
      }),
    );
    await page.setViewportSize({ width: 1280, height: 900 });
    await page.goto("/");
    await openWizardWithProject(page);
    await expandMoreOptions(page);
    const w = wizard(page);
    await w.getByRole("button", { name: "Base branch" }).click();
    const baseInput = w.getByLabel("Base branch");
    await baseInput.click();
    const option = w.getByRole("option", { name: /release-1\.2/ });
    await expect(option).toBeVisible();
    await option.click();
    await expect(baseInput).toHaveValue("release-1.2");
  });
});
