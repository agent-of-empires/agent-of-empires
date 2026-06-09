import { test, expect } from "./helpers/mockedTest";
import { Page } from "@playwright/test";

// Wizard scratch-session stories (#1324), ported from the live suite.
// Covers:
// - the "Skip project folder" toggle rendering above the project tabs;
// - flipping it enabling Next without a path and hiding the path
//   sources (tab strip + directory browser);
// - scratch hiding the worktree controls on the Session step (a
//   scratch directory is not a git repo);
// - Cmd+Shift+N opening the wizard at Review pre-configured for
//   scratch, with Cmd+Enter submitting the scratch-shaped POST;
// - the sidebar collapsing all scratch sessions into one synthetic
//   "Scratch" group (useRepoGroups, #1324 follow-up).

function sessionStub(overrides: Record<string, unknown>) {
  return {
    id: "seed-session",
    title: "seed",
    project_path: "/tmp/example",
    group_path: "/tmp",
    tool: "claude",
    status: "Idle",
    yolo_mode: false,
    created_at: new Date().toISOString(),
    last_accessed_at: new Date().toISOString(),
    last_error: null,
    branch: null,
    main_repo_path: null,
    is_sandboxed: false,
    has_terminal: true,
    profile: "default",
    workspace_repos: [],
    scratch: false,
    ...overrides,
  };
}

async function mockApis(
  page: Page,
  opts: { sessions?: unknown[]; captured?: { body: Record<string, unknown> | null } } = {},
) {
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
    r.fulfill({
      json: [{ name: "claude", binary: "claude", host_only: false, installed: true, install_hint: "" }],
    }),
  );
  await page.route("**/api/sessions", (r) => {
    if (r.request().method() === "POST") {
      if (opts.captured) {
        opts.captured.body = JSON.parse(r.request().postData() || "{}");
      }
      return r.fulfill({ json: { session: { id: "new-session" } } });
    }
    return r.fulfill({
      json: {
        sessions: opts.sessions ?? [sessionStub({})],
        workspace_ordering: [],
      },
    });
  });
}

async function openWizard(page: Page) {
  await page.locator("body").click();
  await page.keyboard.press("n");
  await expect(page.getByRole("heading", { name: "New session" })).toBeVisible();
}

test.describe("Wizard scratch sessions (#1324)", () => {
  test("scratch toggle is visible above the project tabs and defaults off", async ({ page }) => {
    await mockApis(page);
    await page.setViewportSize({ width: 1280, height: 900 });
    await page.goto("/");
    await openWizard(page);

    const toggle = page.getByRole("switch", { name: "Skip project folder" });
    await expect(toggle).toBeVisible();
    await expect(toggle).toHaveAttribute("aria-checked", "false");
  });

  test("scratch toggle enables Next without a path and hides the path sources", async ({ page }) => {
    await mockApis(page);
    await page.setViewportSize({ width: 1280, height: 900 });
    await page.goto("/");
    await openWizard(page);

    // Baseline: Next is disabled because no path is selected.
    const nextButton = page.getByRole("button", { name: "Next" });
    await expect(nextButton).toBeDisabled();

    // Flip the toggle. The reducer also clears any prefilled path /
    // useWorktree state, so Next must transition to enabled.
    await page.getByRole("switch", { name: "Skip project folder" }).click();
    await expect(nextButton).toBeEnabled();

    // The scratch confirmation card replaces the path picker, so the
    // Browse tab must no longer be reachable.
    await expect(page.getByText(/Scratch session/).first()).toBeVisible();
    await expect(page.getByRole("button", { name: "Browse" })).toBeHidden();
  });

  test("scratch hides the worktree section on the Session step", async ({ page }) => {
    await mockApis(page);
    await page.setViewportSize({ width: 1280, height: 900 });
    await page.goto("/");
    await openWizard(page);

    await page.getByRole("switch", { name: "Skip project folder" }).click();
    await page.getByRole("button", { name: "Next" }).click();
    await expect(page.getByRole("heading", { name: "Name your session", exact: true })).toBeVisible();

    // #1514 folds the worktree section (and, for scratch, the
    // explanatory note that replaces it) behind the top-level
    // "Advanced" disclosure that defaults closed; expand it first.
    await page.getByRole("button", { name: "Advanced" }).click();

    await expect(page.getByText(/Scratch sessions do not use git worktrees/)).toBeVisible();
    // The "Create a worktree" switch must NOT be in the DOM at all.
    await expect(page.getByRole("switch", { name: /Create a worktree/i })).toHaveCount(0);
  });

  test("Cmd+Shift+N opens the wizard at Review with scratch on; Cmd+Enter launches", async ({ page }) => {
    // The App.tsx callback sets both scratch: true and skipToReview, so
    // a follow-up Cmd+Enter / Ctrl+Enter creates the session in two
    // keystrokes total.
    const captured: { body: Record<string, unknown> | null } = { body: null };
    await mockApis(page, { captured });
    await page.setViewportSize({ width: 1280, height: 900 });
    await page.goto("/");

    // Wait for the dashboard to be interactive before firing global
    // shortcuts: the sidebar's "New session" button doubles as a proxy
    // for "the document-level keydown handler is registered". Pressing
    // earlier races React's effect registration on a cold CI runner
    // (the live original used the same guard).
    await expect(page.getByRole("button", { name: "New session", exact: true }).first()).toBeVisible();
    await page.keyboard.press("ControlOrMeta+Shift+KeyN");

    await expect(page.getByRole("heading", { name: "Review & Launch" })).toBeVisible();
    await expect(page.getByRole("button", { name: /Launch session/ })).toBeVisible();
    await expect(page.getByText("Scratch directory (provisioned on create)")).toBeVisible();

    await page.keyboard.press("ControlOrMeta+Enter");

    // The POST is scratch-shaped: scratch flag on, no path (the server
    // provisions the directory).
    await expect.poll(() => captured.body?.scratch).toBe(true);
    expect(captured.body?.path).toBe("");
    expect(captured.body?.tool).toBe("claude");
  });

  test("scratch sessions render in a single synthetic Scratch group", async ({ page }) => {
    // Every scratch session lives under its own `<app_dir>/scratch/<id>/`
    // directory, so bucketing by project_path would render N one-session
    // groups. useRepoGroups collapses them into one synthetic "Scratch"
    // group (stable id `__scratch__`), mirroring the multi-repo group.
    await mockApis(page, {
      sessions: [
        sessionStub({
          id: "alpha",
          title: "alpha-session",
          project_path: "/home/user/repo-alpha",
          group_path: "/home/user",
        }),
        sessionStub({
          id: "scr-1",
          title: "scratch-one",
          project_path: "/home/user/.config/agent-of-empires/scratch/scr-1",
          group_path: null,
          scratch: true,
        }),
        sessionStub({
          id: "scr-2",
          title: "scratch-two",
          project_path: "/home/user/.config/agent-of-empires/scratch/scr-2",
          group_path: null,
          scratch: true,
        }),
      ],
    });
    await page.setViewportSize({ width: 1280, height: 900 });
    await page.goto("/");

    // Two groups: the real repo (alpha) + the synthetic "Scratch"
    // group. If grouping regressed, each scratch session would surface
    // as its own header and this assertion would see 3.
    const groupHeaders = page.locator("[data-testid='sidebar-group-header']");
    await expect(groupHeaders).toHaveCount(2);

    await expect(page.getByText("repo-alpha")).toBeVisible();
    const scratchHeader = page.locator("[data-testid='sidebar-group-header'][data-group-id='__scratch__']");
    await expect(scratchHeader).toBeVisible();
    await expect(scratchHeader).toContainText("Scratch");

    // All three session rows are visible: alpha plus both scratch
    // sessions under the synthetic group. Row count proves no rows got
    // dropped by the grouping change.
    const rows = page.locator("[data-testid='sidebar-session-row']");
    await expect(rows).toHaveCount(3);
    await expect(page.getByText("alpha-session")).toBeVisible();
    await expect(page.getByText("scratch-one")).toBeVisible();
    await expect(page.getByText("scratch-two")).toBeVisible();
  });
});
