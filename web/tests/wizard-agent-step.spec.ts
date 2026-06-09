import { test, expect } from "./helpers/mockedTest";
import { Page } from "@playwright/test";

// Wizard Agent step (#1219). Covers the agent picker grid, the profile
// selector visibility rule (only when profiles.length > 1) plus its
// APPLY_PROFILE_DEFAULTS path, the sandbox toggle (which is disabled
// when Docker is unavailable), and the Advanced section's extra-env /
// extra-args inputs. Profile-defaults seeding on mount is exercised by
// the unit tests on the reducer; here we test the picker-driven UI path.

interface MockOptions {
  agents?: Array<{
    name: string;
    binary?: string;
    host_only?: boolean;
    installed?: boolean;
    install_hint?: string;
  }>;
  profiles?: Array<{ name: string; is_default: boolean; description?: string }>;
  docker?: boolean;
  profileSettings?: Record<string, unknown>;
}

async function mockApis(page: Page, opts: MockOptions = {}) {
  await page.route("**/api/login/status", (r) => r.fulfill({ json: { required: false, authenticated: true } }));
  for (const path of ["themes", "groups", "devices", "about", "system/update-status"]) {
    await page.route(`**/api/${path}`, (r) =>
      r.fulfill({
        json: path === "about" || path === "system/update-status" ? {} : [],
      }),
    );
  }
  await page.route("**/api/settings**", (r) => r.fulfill({ json: opts.profileSettings ?? {} }));
  await page.route("**/api/profiles", (r) => r.fulfill({ json: opts.profiles ?? [] }));
  await page.route("**/api/docker/status", (r) =>
    r.fulfill({
      json: {
        available: opts.docker ?? false,
        runtime: opts.docker ? "docker" : null,
      },
    }),
  );
  await page.route("**/api/agents", (r) =>
    r.fulfill({
      json: opts.agents ?? [
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

async function openAgentStep(page: Page) {
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
}

test.describe("Wizard agent step (#1219)", () => {
  test("agent picker renders installed agents, including terminal fallback tools", async ({ page }) => {
    await mockApis(page, {
      agents: [
        { name: "claude", installed: true, host_only: false },
        { name: "codex", installed: true, host_only: false },
        { name: "antigravity", installed: true, host_only: false },
        {
          name: "uninstalled-tool",
          installed: false,
          host_only: false,
          install_hint: "brew install x",
        },
      ],
    });
    await page.setViewportSize({ width: 1280, height: 900 });
    await page.goto("/");
    await openAgentStep(page);
    const claudeBtn = page.getByRole("button", { name: "claude", exact: true });
    const codexBtn = page.getByRole("button", { name: "codex", exact: true });
    const antigravityBtn = page.getByRole("button", {
      name: "antigravity",
      exact: true,
    });
    await expect(claudeBtn).toBeVisible();
    await expect(codexBtn).toBeVisible();
    await expect(antigravityBtn).toBeVisible();
    // Uninstalled agents are hidden from the picker grid.
    await expect(page.getByRole("button", { name: "uninstalled-tool", exact: true })).toHaveCount(0);
  });

  test("profile picker is hidden when there is only one profile", async ({ page }) => {
    await mockApis(page, { profiles: [{ name: "default", is_default: true }] });
    await page.setViewportSize({ width: 1280, height: 900 });
    await page.goto("/");
    await openAgentStep(page);
    await expect(page.getByText("Workflow preset")).toHaveCount(0);
  });

  test("profile picker visible with multiple profiles; selecting applies sandbox + yolo defaults", async ({ page }) => {
    await mockApis(page, {
      docker: true,
      profiles: [
        { name: "default", is_default: true },
        { name: "yolo-sandbox", is_default: false },
      ],
    });
    // Track /api/settings calls keyed by the ?profile query param so we can
    // prove the picker (not just boot-time seeding) hit the endpoint with
    // the selected profile.
    const settingsCalls: string[] = [];
    await page.route("**/api/settings**", (r) => {
      const url = new URL(r.request().url());
      const profile = url.searchParams.get("profile");
      settingsCalls.push(profile ?? "");
      if (profile === "yolo-sandbox") {
        return r.fulfill({
          json: {
            sandbox: { enabled_by_default: true, environment: ["FOO=bar"] },
            session: { yolo_mode_default: true, default_tool: "claude" },
          },
        });
      }
      return r.fulfill({ json: {} });
    });
    await page.setViewportSize({ width: 1280, height: 900 });
    await page.goto("/");
    await openAgentStep(page);
    await expect(page.getByText("Workflow preset")).toBeVisible();
    // Profile picker is now a radiogroup of cards (#949) so each profile
    // can show a short description; clicking the row is equivalent to the
    // old <select>.selectOption call.
    await page.getByRole("radio", { name: /yolo-sandbox/ }).click();
    // Both toggles flip on because APPLY_PROFILE_DEFAULTS dispatched.
    const sandboxToggle = page.locator("label", { hasText: "Run in a safe container" }).locator("role=switch");
    const yoloToggle = page.locator("label", { hasText: "Auto-approve actions" }).locator("role=switch");
    await expect(sandboxToggle).toHaveAttribute("aria-checked", "true");
    await expect(yoloToggle).toHaveAttribute("aria-checked", "true");
    expect(settingsCalls).toContain("yolo-sandbox");
  });

  test("profile picker renders description as helper text under each option (#949)", async ({ page }) => {
    await mockApis(page, {
      profiles: [
        {
          name: "default",
          is_default: true,
          description: "Stock setup with no overrides",
        },
        {
          name: "yolo-sandbox",
          is_default: false,
          description: "Auto-approve in a container",
        },
        // Profiles without a description should still render (just without
        // helper text), so a mixed list does not silently drop them.
        { name: "no-desc", is_default: false },
      ],
    });
    await page.setViewportSize({ width: 1280, height: 900 });
    await page.goto("/");
    await openAgentStep(page);
    await expect(page.getByText("Workflow preset")).toBeVisible();
    // Descriptions render beneath the profile name.
    await expect(page.getByText("Stock setup with no overrides")).toBeVisible();
    await expect(page.getByText("Auto-approve in a container")).toBeVisible();
    // Profiles without a description still appear in the picker.
    await expect(page.getByRole("radio", { name: /no-desc/ })).toBeVisible();
  });

  test("sandbox toggle is disabled when Docker is not running", async ({ page }) => {
    await mockApis(page, { docker: false });
    await page.setViewportSize({ width: 1280, height: 900 });
    await page.goto("/");
    await openAgentStep(page);
    const sandboxToggle = page.locator("label", { hasText: "Run in a safe container" }).locator("role=switch");
    await expect(sandboxToggle).toBeDisabled();
    await expect(page.getByText("Docker is not running.")).toBeVisible();
  });

  test("Advanced settings: extra args propagate to the create-session POST body", async ({ page }) => {
    await mockApis(page);
    let captured: { extra_args?: string } | null = null;
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
    await openAgentStep(page);
    await page.getByRole("button", { name: "Advanced settings" }).click();
    await page.getByPlaceholder("e.g. --port 8080").fill("--verbose");
    await page.getByRole("button", { name: /Next/ }).click();
    await page.getByRole("button", { name: /Launch session/ }).click();
    await expect.poll(() => captured?.extra_args).toBe("--verbose");
  });
});
