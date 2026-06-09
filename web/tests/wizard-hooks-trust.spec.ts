import { test, expect } from "./helpers/mockedTest";
import { Page } from "@playwright/test";

// Wizard on_create hooks-trust confirmation modal (#2066). Creating a session
// in a repo whose `.agent-of-empires/config.toml` defines on_create hooks that
// need approval makes the server return a `hooks_need_trust` 403. The wizard
// must surface a confirm modal listing the commands, then resubmit with
// `trust_hooks: true` on Proceed. Covers: modal shows the commands, Cancel
// aborts, and Proceed retries with trust_hooks and creates.

interface Calls {
  createWithoutTrust: number;
  createWithTrust: number;
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
  await page.route("**/api/docker/status", (r) => r.fulfill({ json: { available: false, runtime: null } }));
  await page.route("**/api/agents", (r) =>
    r.fulfill({
      json: [{ name: "claude", binary: "claude", host_only: false, installed: true, install_hint: "" }],
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
    const body = JSON.parse(r.request().postData() || "{}");
    if (body.trust_hooks === true) {
      calls.createWithTrust += 1;
      return r.fulfill({ json: { session: { id: "new-session" } } });
    }
    calls.createWithoutTrust += 1;
    return r.fulfill({
      status: 403,
      json: {
        error: "hooks_need_trust",
        message: "Repository hooks require trust. Resubmit with trust_hooks: true to approve.",
        on_create: ["bash scripts/setup-worktree.sh", "cp .env.example .env"],
        on_launch: ["npm run dev-seed"],
        on_destroy: [],
        needs_mcp_trust: false,
      },
    });
  });
}

// Walk project -> session -> agent -> review and click Launch. Lands with the
// create paused on the hooks-trust modal (server returned hooks_need_trust).
async function launchSession(page: Page) {
  await page.locator("body").click();
  await page.keyboard.press("n");
  await expect(page.getByRole("heading", { name: "New session" })).toBeVisible();
  const recent = page.getByRole("button").filter({ hasText: "/tmp/example" }).first();
  await recent.waitFor({ state: "visible", timeout: 5000 });
  await recent.click();
  await page.getByRole("button", { name: "Next" }).click();
  await expect(page.getByText("Name your session")).toBeVisible();
  await page.getByRole("button", { name: "Next" }).click();
  await expect(page.getByRole("heading", { name: "Which AI agent?" })).toBeVisible();
  await page.getByRole("button", { name: "Next" }).click();
  await expect(page.getByRole("heading", { name: "Review & Launch" })).toBeVisible();
  await page.getByRole("button", { name: /Launch session/ }).click();
}

test.describe("Wizard on_create hooks-trust confirmation (#2066)", () => {
  test("modal shows the on_create commands before creating", async ({ page }) => {
    const calls: Calls = { createWithoutTrust: 0, createWithTrust: 0 };
    await mockApis(page, calls);
    await page.setViewportSize({ width: 1280, height: 900 });
    await page.goto("/");
    await launchSession(page);

    const dialog = page.getByTestId("hooks-trust-dialog");
    await expect(dialog).toBeVisible();
    await expect(dialog).toContainText("bash scripts/setup-worktree.sh");
    await expect(dialog).toContainText("cp .env.example .env");
    // Approval trusts the whole hooks hash, so on_launch is listed too.
    await expect(dialog).toContainText("npm run dev-seed");
    // The initial (untrusted) attempt was refused; nothing trusted yet.
    await expect.poll(() => calls.createWithoutTrust).toBe(1);
    expect(calls.createWithTrust).toBe(0);
  });

  test("Cancel aborts the create and returns to the wizard", async ({ page }) => {
    const calls: Calls = { createWithoutTrust: 0, createWithTrust: 0 };
    await mockApis(page, calls);
    await page.setViewportSize({ width: 1280, height: 900 });
    await page.goto("/");
    await launchSession(page);

    await expect(page.getByTestId("hooks-trust-dialog")).toBeVisible();
    await page.getByRole("button", { name: "Cancel" }).click();
    await expect(page.getByTestId("hooks-trust-dialog")).toHaveCount(0);
    expect(calls.createWithTrust).toBe(0);
    await expect(page.getByRole("button", { name: /Launch session/ })).toBeEnabled();
  });

  test("Proceed resubmits with trust_hooks and creates the session", async ({ page }) => {
    const calls: Calls = { createWithoutTrust: 0, createWithTrust: 0 };
    await mockApis(page, calls);
    await page.setViewportSize({ width: 1280, height: 900 });
    await page.goto("/");
    await launchSession(page);

    await expect(page.getByTestId("hooks-trust-dialog")).toBeVisible();
    await page.getByTestId("hooks-trust-proceed").click();
    await expect.poll(() => calls.createWithTrust).toBe(1);
  });
});
