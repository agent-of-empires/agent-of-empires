import { test, expect } from "./helpers/mockedTest";
import { Page } from "@playwright/test";
import { openWizard, selectProject, expandMoreOptions, launch, wizard } from "./helpers/wizard";

// Single-screen new-session wizard acceptance stories (#2210). The 4-step
// wizard collapsed into one screen: project + agent + structured-view +
// Launch are always visible; everything else folds behind "More options";
// the Review step is gone.

const SEED_SESSION = {
  id: "seed-session",
  title: "seed",
  project_path: "/tmp/example",
  group_path: "/tmp",
  tool: "claude",
  status: "Idle",
  yolo_mode: false,
  created_at: "2026-01-01T00:00:00Z",
  last_accessed_at: "2026-01-01T00:00:00Z",
  last_error: null,
  branch: null,
  main_repo_path: null,
  is_sandboxed: false,
  has_terminal: true,
  profile: "default",
  workspace_repos: [],
};

async function mockApis(page: Page, captured?: { body: Record<string, unknown> | null }) {
  await page.route("**/api/login/status", (r) => r.fulfill({ json: { required: false, authenticated: true } }));
  for (const path of ["settings", "themes", "profiles", "groups", "devices", "about", "system/update-status"]) {
    await page.route(`**/api/${path}`, (r) =>
      r.fulfill({
        json: path === "settings" || path === "about" || path === "system/update-status" ? {} : [],
      }),
    );
  }
  await page.route("**/api/docker/status", (r) => r.fulfill({ json: { available: false, runtime: null } }));
  await page.route("**/api/agents", (r) =>
    r.fulfill({ json: [{ name: "claude", binary: "claude", host_only: false, installed: true, install_hint: "" }] }),
  );
  await page.route("**/api/sessions", (r) => {
    if (r.request().method() === "POST") {
      if (captured) captured.body = JSON.parse(r.request().postData() || "{}");
      return r.fulfill({ json: { session: { id: "new-session" } } });
    }
    return r.fulfill({ json: { sessions: [SEED_SESSION], workspace_ordering: [] } });
  });
}

test.describe("Single-screen wizard (#2210)", () => {
  test("recent project + default agent launches with one click, no paging", async ({ page }) => {
    // Story: pick a recent project, hit Launch, no Next steps.
    const captured: { body: Record<string, unknown> | null } = { body: null };
    await mockApis(page, captured);
    await page.setViewportSize({ width: 1280, height: 900 });
    await page.goto("/");

    await openWizard(page);
    await selectProject(page, "/tmp/example");
    // No "Next" button exists anymore.
    await expect(page.getByRole("button", { name: "Next" })).toHaveCount(0);
    await launch(page);

    await expect.poll(() => captured.body?.path).toBe("/tmp/example");
    expect(captured.body?.tool).toBe("claude");
  });

  test("only essentials show on open; advanced controls hide behind collapsed More options", async ({ page }) => {
    // Story: project + session title + agent picker + Launch are visible;
    // structured-view / worktree / sandbox / preset sit inside a collapsed
    // fold.
    await mockApis(page);
    await page.setViewportSize({ width: 1280, height: 900 });
    await page.goto("/");

    await openWizard(page);
    const w = wizard(page);
    await expect(w.getByRole("button", { name: /^claude/ })).toBeVisible();
    await expect(w.getByPlaceholder("Auto-generated if empty")).toBeVisible();
    await expect(w.getByRole("button", { name: /Launch session/ })).toBeVisible();

    const more = w.getByRole("button", { name: "More options" });
    await expect(more).toHaveAttribute("aria-expanded", "false");
    await expect(w.getByRole("switch", { name: "Use structured view" })).toHaveCount(0);
    await expect(w.getByText("Create a worktree")).toHaveCount(0);
    await expect(w.getByText("Run in a safe container")).toHaveCount(0);

    await expandMoreOptions(page);
    await expect(w.getByRole("switch", { name: "Use structured view" })).toBeVisible();
    await expect(w.getByText("Create a worktree")).toBeVisible();
    await expect(w.getByText("Run in a safe container")).toBeVisible();
  });

  test("worktree branch set under More options flows into the create payload", async ({ page }) => {
    // Story: expand More options, set a worktree branch, launch; the
    // created session uses that branch (no Review step in between).
    const captured: { body: Record<string, unknown> | null } = { body: null };
    await mockApis(page, captured);
    await page.setViewportSize({ width: 1280, height: 900 });
    await page.goto("/");

    await openWizard(page);
    await selectProject(page, "/tmp/example");
    await expandMoreOptions(page);
    await page.getByPlaceholder("Uses session title if empty").fill("my-feature-branch");
    await launch(page);

    await expect.poll(() => captured.body?.worktree_branch).toBe("my-feature-branch");
    expect(captured.body?.create_new_branch).toBe(true);
  });

  test("Launch button shows the submitting state while the create POST is in flight", async ({ page }) => {
    await page.route("**/api/login/status", (r) => r.fulfill({ json: { required: false, authenticated: true } }));
    for (const path of ["settings", "themes", "profiles", "groups", "devices", "about", "system/update-status"]) {
      await page.route(`**/api/${path}`, (r) =>
        r.fulfill({ json: path === "settings" || path === "about" || path === "system/update-status" ? {} : [] }),
      );
    }
    await page.route("**/api/docker/status", (r) => r.fulfill({ json: { available: false, runtime: null } }));
    await page.route("**/api/agents", (r) =>
      r.fulfill({ json: [{ name: "claude", binary: "claude", host_only: false, installed: true, install_hint: "" }] }),
    );
    let resolveCreate: (() => void) | null = null;
    const createPromise = new Promise<void>((res) => {
      resolveCreate = res;
    });
    await page.route("**/api/sessions", async (r) => {
      if (r.request().method() === "POST") {
        await createPromise;
        return r.fulfill({ json: { session: { id: "new-session" } } });
      }
      return r.fulfill({ json: { sessions: [SEED_SESSION], workspace_ordering: [] } });
    });
    await page.setViewportSize({ width: 1280, height: 900 });
    await page.goto("/");

    await openWizard(page);
    await selectProject(page, "/tmp/example");
    const launchBtn = page.getByRole("button", { name: /Launch session/ });
    await expect(launchBtn).toBeEnabled();
    await launchBtn.click();
    await expect(page.getByText("Creating session...")).toBeVisible();
    resolveCreate?.();
  });
});
