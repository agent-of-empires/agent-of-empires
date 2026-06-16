import { test, expect } from "./helpers/mockedTest";
import { Page } from "@playwright/test";
import { openWizard, selectProject, expandMoreOptions, launch, wizard } from "./helpers/wizard";

// Wizard glob volume_ignores confirmation modal (#2045). A sandbox session
// whose resolved config has glob volume_ignores (e.g. `**/bin`) must surface a
// one-time snapshot-expansion confirm modal before creating: the patterns are
// expanded against the workspace at create time, a snapshot that won't shadow
// directories a build creates later inside the container. Covers: modal shows
// with patterns + match count, Cancel aborts the create, Proceed creates, and
// "Don't show this again" hits the acknowledge endpoint.

interface Calls {
  createSession: number;
  acknowledge: number;
}

async function mockApis(page: Page, calls: Calls) {
  await page.route("**/api/login/status", (r) => r.fulfill({ json: { required: false, authenticated: true } }));
  for (const path of ["themes", "groups", "devices", "about", "system/update-status"]) {
    await page.route(`**/api/${path}`, (r) =>
      r.fulfill({ json: path === "about" || path === "system/update-status" ? {} : [] }),
    );
  }
  await page.route("**/api/settings**", (r) => r.fulfill({ json: {} }));
  await page.route("**/api/profiles", (r) => r.fulfill({ json: [] }));
  await page.route("**/api/recent-projects", (r) => r.fulfill({ json: { projects: [] } }));
  await page.route("**/api/projects", (r) => r.fulfill({ json: [] }));
  await page.route("**/api/docker/status", (r) => r.fulfill({ json: { available: true, runtime: "docker" } }));
  await page.route("**/api/agents", (r) =>
    r.fulfill({
      json: [{ name: "claude", binary: "claude", host_only: false, installed: true, install_hint: "" }],
    }),
  );
  // Glob preview: two patterns, three matched dirs, not yet acknowledged.
  await page.route("**/api/sandbox/volume-ignores-preview**", (r) =>
    r.fulfill({
      json: {
        acknowledged: false,
        globs: [
          { pattern: "**/bin", matched_paths: ["/workspace/example/src/App/bin", "/workspace/example/tests/Lib/bin"] },
          { pattern: "**/obj", matched_paths: ["/workspace/example/src/App/obj"] },
        ],
      },
    }),
  );
  await page.route("**/api/app-state/volume-ignores-globs-acknowledged", (r) => {
    if (r.request().method() === "POST") calls.acknowledge += 1;
    return r.fulfill({ json: { has_acknowledged_volume_ignores_globs: true } });
  });
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
    calls.createSession += 1;
    return r.fulfill({ json: { session: { id: "new-session" } } });
  });
}

// Pick the recent project, enable the sandbox toggle under More options, then
// click Launch. Lands with the create paused on the glob modal.
async function launchSandboxSession(page: Page) {
  await openWizard(page);
  await selectProject(page, "/tmp/example");
  await expandMoreOptions(page);
  const sandboxToggle = wizard(page).locator("label", { hasText: "Run in a safe container" }).locator("role=switch");
  await sandboxToggle.click();
  await expect(sandboxToggle).toHaveAttribute("aria-checked", "true");
  await launch(page);
}

test.describe("Wizard glob volume_ignores confirmation (#2045)", () => {
  test("modal shows the patterns and match count before creating", async ({ page }) => {
    const calls: Calls = { createSession: 0, acknowledge: 0 };
    await mockApis(page, calls);
    await page.setViewportSize({ width: 1280, height: 900 });
    await page.goto("/");
    await launchSandboxSession(page);

    const dialog = page.getByTestId("volume-ignores-glob-dialog");
    await expect(dialog).toBeVisible();
    await expect(dialog).toContainText("**/bin");
    await expect(dialog).toContainText("**/obj");
    await expect(dialog).toContainText("3 directories");
    // The session is not created until the user proceeds.
    expect(calls.createSession).toBe(0);
  });

  test("Cancel aborts the create and returns to the wizard", async ({ page }) => {
    const calls: Calls = { createSession: 0, acknowledge: 0 };
    await mockApis(page, calls);
    await page.setViewportSize({ width: 1280, height: 900 });
    await page.goto("/");
    await launchSandboxSession(page);

    await expect(page.getByTestId("volume-ignores-glob-dialog")).toBeVisible();
    await page.getByRole("button", { name: "Cancel" }).click();
    await expect(page.getByTestId("volume-ignores-glob-dialog")).toHaveCount(0);
    expect(calls.createSession).toBe(0);
    expect(calls.acknowledge).toBe(0);
    // Wizard is still open and the Launch button is interactive again.
    await expect(wizard(page).getByRole("button", { name: /Launch session/ })).toBeEnabled();
  });

  test("Proceed without the checkbox creates the session and does not acknowledge", async ({ page }) => {
    const calls: Calls = { createSession: 0, acknowledge: 0 };
    await mockApis(page, calls);
    await page.setViewportSize({ width: 1280, height: 900 });
    await page.goto("/");
    await launchSandboxSession(page);

    await expect(page.getByTestId("volume-ignores-glob-dialog")).toBeVisible();
    await page.getByTestId("volume-ignores-glob-proceed").click();
    await expect.poll(() => calls.createSession).toBe(1);
    expect(calls.acknowledge).toBe(0);
  });

  test("'Don't show again' + Proceed persists the acknowledgment and creates", async ({ page }) => {
    const calls: Calls = { createSession: 0, acknowledge: 0 };
    await mockApis(page, calls);
    await page.setViewportSize({ width: 1280, height: 900 });
    await page.goto("/");
    await launchSandboxSession(page);

    await expect(page.getByTestId("volume-ignores-glob-dialog")).toBeVisible();
    await page.getByTestId("volume-ignores-glob-dont-show-again").click();
    await page.getByTestId("volume-ignores-glob-proceed").click();
    await expect.poll(() => calls.acknowledge).toBe(1);
    await expect.poll(() => calls.createSession).toBe(1);
  });
});
