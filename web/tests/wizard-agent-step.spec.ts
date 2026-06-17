import { test, expect } from "./helpers/mockedTest";
import { Page } from "@playwright/test";
import { openWizard, selectProject, expandMoreOptions, launch, wizard } from "./helpers/wizard";

// Wizard agent section (#1219, migrated to the single-screen wizard #2210).
// Covers the always-visible agent picker grid, plus the agent OPTIONS that
// now live behind "More options": the profile selector visibility rule
// (only when profiles.length > 1) and its APPLY_PROFILE_DEFAULTS path, the
// sandbox toggle (disabled when Docker is unavailable), and the extra-args
// input flowing into the create-session POST body. Profile-defaults seeding
// on mount is exercised by the reducer unit tests; here we test the
// picker-driven UI path.

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

function seedSessionsPayload() {
  return {
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
  };
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
  // ProjectStep always mounts now and fetches these.
  await page.route("**/api/recent-projects", (r) => r.fulfill({ json: { projects: [] } }));
  await page.route("**/api/projects", (r) => r.fulfill({ json: [] }));
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
      return r.fulfill({ json: seedSessionsPayload() });
    }
    return r.fulfill({ json: { session: { id: "new-session" } } });
  });
}

test.describe("Wizard agent section (#1219)", () => {
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
    await openWizard(page);
    await selectProject(page, "/tmp/example");
    const w = wizard(page);
    // Picker is always visible; no step navigation.
    await expect(w.getByRole("button", { name: "claude", exact: true })).toBeVisible();
    await expect(w.getByRole("button", { name: "codex", exact: true })).toBeVisible();
    await expect(w.getByRole("button", { name: "antigravity", exact: true })).toBeVisible();
    // Uninstalled agents are hidden from the picker grid.
    await expect(w.getByRole("button", { name: "uninstalled-tool", exact: true })).toHaveCount(0);
  });

  test("profile picker is hidden when there is only one profile", async ({ page }) => {
    await mockApis(page, { profiles: [{ name: "default", is_default: true }] });
    await page.setViewportSize({ width: 1280, height: 900 });
    await page.goto("/");
    await openWizard(page);
    await selectProject(page, "/tmp/example");
    await expandMoreOptions(page);
    await expect(wizard(page).getByText("Workflow preset")).toHaveCount(0);
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
    await openWizard(page);
    await selectProject(page, "/tmp/example");
    await expandMoreOptions(page);
    const w = wizard(page);
    await expect(w.getByText("Workflow preset")).toBeVisible();
    // Profile picker is a radiogroup of cards (#949) so each profile can
    // show a short description; clicking the row is equivalent to the old
    // <select>.selectOption call.
    await w.getByRole("radio", { name: /yolo-sandbox/ }).click();
    // Both toggles flip on because APPLY_PROFILE_DEFAULTS dispatched.
    const sandboxToggle = w.locator("label", { hasText: "Run in a safe container" }).locator("role=switch");
    const yoloToggle = w.locator("label", { hasText: "Auto-approve actions" }).locator("role=switch");
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
    await openWizard(page);
    await selectProject(page, "/tmp/example");
    await expandMoreOptions(page);
    const w = wizard(page);
    await expect(w.getByText("Workflow preset")).toBeVisible();
    // Descriptions render beneath the profile name.
    await expect(w.getByText("Stock setup with no overrides")).toBeVisible();
    await expect(w.getByText("Auto-approve in a container")).toBeVisible();
    // Profiles without a description still appear in the picker.
    await expect(w.getByRole("radio", { name: /no-desc/ })).toBeVisible();
  });

  test("sandbox toggle is disabled when Docker is not running", async ({ page }) => {
    await mockApis(page, { docker: false });
    await page.setViewportSize({ width: 1280, height: 900 });
    await page.goto("/");
    await openWizard(page);
    await selectProject(page, "/tmp/example");
    await expandMoreOptions(page);
    const w = wizard(page);
    const sandboxToggle = w.locator("label", { hasText: "Run in a safe container" }).locator("role=switch");
    await expect(sandboxToggle).toBeDisabled();
    await expect(w.getByText("Docker is not running.")).toBeVisible();
  });

  test("extra args propagate to the create-session POST body", async ({ page }) => {
    await mockApis(page);
    let captured: { extra_args?: string } | null = null;
    await page.route("**/api/sessions", (r) => {
      if (r.request().method() === "POST") {
        captured = JSON.parse(r.request().postData() || "{}");
        return r.fulfill({ json: { session: { id: "new-session" } } });
      }
      return r.fulfill({ json: seedSessionsPayload() });
    });
    await page.setViewportSize({ width: 1280, height: 900 });
    await page.goto("/");
    await openWizard(page);
    await selectProject(page, "/tmp/example");
    await expandMoreOptions(page);
    // The advanced launch knobs render flat under More options on the
    // single screen, so the extra-args input is directly available (no
    // nested "Advanced settings" disclosure).
    await wizard(page).getByPlaceholder("e.g. --port 8080").fill("--verbose");
    await launch(page);
    await expect.poll(() => captured?.extra_args).toBe("--verbose");
  });
});
