import { test, expect } from "./helpers/mockedTest";
import { Page } from "@playwright/test";

// Wizard "Attach to existing branch" toggle (#969). Mirrors the TUI's
// `Attach to existing branch:` checkbox: when on, the request body
// sends `create_new_branch: false` (and the Advanced base-branch
// section hides since it's only honored for new-branch creates).

async function mockApis(page: Page) {
  await page.route("**/api/login/status", (r) =>
    r.fulfill({ json: { required: false, authenticated: true } }),
  );
  for (const path of [
    "settings",
    "themes",
    "profiles",
    "groups",
    "devices",
    "about",
    "system/update-status",
  ]) {
    await page.route(`**/api/${path}`, (r) =>
      r.fulfill({
        json:
          path === "settings" ||
          path === "about" ||
          path === "system/update-status"
            ? {}
            : [],
      }),
    );
  }
  await page.route("**/api/docker/status", (r) =>
    r.fulfill({ json: { available: false, runtime: null } }),
  );
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

async function openSessionStep(page: Page) {
  await page.locator("body").click();
  await page.keyboard.press("n");
  await expect(
    page.getByRole("heading", { name: "New session" }),
  ).toBeVisible();
  const recent = page
    .getByRole("button")
    .filter({ hasText: "/tmp/example" })
    .first();
  await recent.waitFor({ state: "visible", timeout: 5000 });
  await recent.click();
  const next = page.getByRole("button", { name: "Next" });
  await expect(next).toBeEnabled();
  await next.click();
  await expect(page.getByText("Name your session")).toBeVisible();
}

// #1514 folds the worktree controls (including the attach-existing
// toggle and the Base branch picker) behind a top-level "Advanced"
// disclosure that defaults closed. Expand it before asserting on them.
async function expandAdvanced(page: Page) {
  await page.getByRole("button", { name: "Advanced" }).click();
}

test.describe("Wizard attach-existing toggle (#969)", () => {
  test("toggle is off by default; Base branch section visible", async ({
    page,
  }) => {
    await mockApis(page);
    await page.setViewportSize({ width: 1280, height: 900 });
    await page.goto("/");
    await openSessionStep(page);
    await expandAdvanced(page);
    const attachToggle = page
      .locator("label", { hasText: "Attach to existing branch" })
      .locator("role=switch");
    await expect(attachToggle).toBeVisible();
    await expect(attachToggle).toHaveAttribute("aria-checked", "false");
    // The base-branch picker (only meaningful for new-branch creates) is
    // its own "Base branch" disclosure inside Advanced.
    await expect(
      page.getByRole("button", { name: "Base branch" }),
    ).toBeVisible();
  });

  test("turning attach on hides the Base branch section", async ({ page }) => {
    await mockApis(page);
    await page.setViewportSize({ width: 1280, height: 900 });
    await page.goto("/");
    await openSessionStep(page);
    await expandAdvanced(page);
    const attachToggle = page
      .locator("label", { hasText: "Attach to existing branch" })
      .locator("role=switch");
    await attachToggle.click();
    await expect(attachToggle).toHaveAttribute("aria-checked", "true");
    await expect(page.getByRole("button", { name: "Base branch" })).toHaveCount(
      0,
    );
  });

  test("submit with attach off sends create_new_branch=true", async ({
    page,
  }) => {
    await mockApis(page);
    let captured: { create_new_branch?: boolean; base_branch?: string } | null =
      null;
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
    await openSessionStep(page);
    await expandAdvanced(page);
    await page.getByPlaceholder("Uses session title if empty").fill("feat/new");
    await page.getByRole("button", { name: /Next/ }).click();
    // Agent step → Next (defaults already set)
    await page.getByRole("button", { name: /Next/ }).click();
    // Review step → Create
    await page.getByRole("button", { name: /Launch session/ }).click();
    await expect.poll(() => captured?.create_new_branch).toBe(true);
  });

  test("submit with attach on sends create_new_branch=false and no base_branch", async ({
    page,
  }) => {
    await mockApis(page);
    let captured: { create_new_branch?: boolean; base_branch?: string } | null =
      null;
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
    await openSessionStep(page);
    await expandAdvanced(page);
    await page
      .getByPlaceholder("Uses session title if empty")
      .fill("feat/existing");
    await page
      .locator("label", { hasText: "Attach to existing branch" })
      .locator("role=switch")
      .click();
    await page.getByRole("button", { name: /Next/ }).click();
    await page.getByRole("button", { name: /Next/ }).click();
    await page.getByRole("button", { name: /Launch session/ }).click();
    await expect.poll(() => captured?.create_new_branch).toBe(false);
    expect(captured?.base_branch).toBeUndefined();
  });
});
