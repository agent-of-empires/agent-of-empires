import { test, expect } from "./helpers/mockedTest";
import { Page } from "@playwright/test";

// Wizard form UI stories ported from the live suite. Covers:
// - branch auto-derivation from the title on the Review step
//   (getReviewSummary falls back branch = worktreeBranch || title);
// - the group-level "New session in <group>" sidebar button prefilling
//   the wizard and skipping straight to Review;
// - last-picked agent persistence across reloads via the
//   "aoe-acp-last-tool" localStorage key (#1133 / #1135);
// - Cmd/Ctrl+Enter on the Review step submitting the create-session
//   POST (ReviewStep's window-level keydown handler).

interface AgentStub {
  name: string;
  binary: string;
  host_only: boolean;
  installed: boolean;
  install_hint: string;
}

const CLAUDE_AGENT: AgentStub = {
  name: "claude",
  binary: "claude",
  host_only: false,
  installed: true,
  install_hint: "",
};
const CODEX_AGENT: AgentStub = { name: "codex", binary: "codex", host_only: false, installed: true, install_hint: "" };

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
        last_accessed_at: new Date().toISOString(),
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

async function mockApis(
  page: Page,
  opts: { agents?: AgentStub[]; captured?: { body: Record<string, unknown> | null } } = {},
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
  await page.route("**/api/agents", (r) => r.fulfill({ json: opts.agents ?? [CLAUDE_AGENT] }));
  await page.route("**/api/sessions", (r) => {
    if (r.request().method() === "POST") {
      if (opts.captured) {
        opts.captured.body = JSON.parse(r.request().postData() || "{}");
      }
      return r.fulfill({ json: { session: { id: "new-session" } } });
    }
    return r.fulfill({ json: seedSessionsPayload() });
  });
}

async function openWizard(page: Page) {
  await page.locator("body").click();
  await page.keyboard.press("n");
  await expect(page.getByRole("heading", { name: "New session" })).toBeVisible();
}

// Walk project -> session -> agent -> review using the seeded recent
// project. Leaves the title blank unless one is provided.
async function openReviewStep(page: Page, title = "") {
  await openWizard(page);
  const recent = page.getByRole("button").filter({ hasText: "/tmp/example" }).first();
  await recent.waitFor({ state: "visible", timeout: 5000 });
  await recent.click();
  await page.getByRole("button", { name: "Next" }).click();
  await expect(page.getByText("Name your session")).toBeVisible();
  if (title) {
    await page.getByPlaceholder("Auto-generated if empty").fill(title);
  }
  await page.getByRole("button", { name: "Next" }).click();
  await expect(page.getByText("Which AI agent?")).toBeVisible();
  await page.getByRole("button", { name: "Next" }).click();
  await expect(page.getByRole("heading", { name: "Review & Launch" })).toBeVisible();
}

// Match a review-card row by its exact label span (same trick as
// wizard-review-step.spec.ts): the row markup is
// `<button><span>{label}</span><span>{value}</span></button>`.
function reviewRow(page: Page, label: string) {
  return page.locator("button").filter({
    has: page.getByText(label, { exact: true }),
  });
}

test.describe("Wizard form UI stories", () => {
  test("editing the title with a blank branch derives the branch on Review", async ({ page }) => {
    // getReviewSummary falls back: branch = worktreeBranch || title ||
    // "Auto-generated". The Branch / worktree EditableRow renders
    // summary.branch when the user never typed a branch.
    await mockApis(page);
    await page.setViewportSize({ width: 1280, height: 900 });
    await page.goto("/");
    await openReviewStep(page);

    await reviewRow(page, "Title").click();
    const input = page.locator("input[type=text]").last();
    await expect(input).toBeFocused();
    await input.fill("autogen-branch-here");
    await input.press("Enter");

    await expect(reviewRow(page, "Title")).toContainText("autogen-branch-here");
    await expect(reviewRow(page, "Branch / worktree")).toContainText("autogen-branch-here");
  });

  test("group-level New session button prefills the wizard and skips to Review", async ({ page }) => {
    // WorkspaceSidebar group headers render a per-group "New session in
    // <group>" button. Clicking it routes through App.tsx's
    // handleCreateSession, which sets wizardPrefill { path, skipToReview }
    // so the wizard lands directly on Review with the repo path filled.
    await mockApis(page);
    await page.setViewportSize({ width: 1280, height: 900 });
    await page.goto("/");

    const groupHeader = page.locator('[data-testid="sidebar-group-header"]').first();
    await expect(groupHeader).toBeVisible();
    await groupHeader.getByRole("button", { name: /New session in /i }).click();

    await expect(page.getByRole("heading", { name: "Review & Launch" })).toBeVisible();
    await expect(reviewRow(page, "Project")).toContainText("/tmp/example");
  });

  test("wizard remembers the last-picked agent across reloads", async ({ page }) => {
    // SessionWizard persists data.tool to localStorage key
    // "aoe-acp-last-tool" on submit success; buildInitialData() reads it
    // back on the next fresh open. Pick a non-default tool so a broken
    // save/restore cannot pass falsely via the "claude" fallback.
    const captured: { body: Record<string, unknown> | null } = { body: null };
    await mockApis(page, { agents: [CLAUDE_AGENT, CODEX_AGENT], captured });
    await page.setViewportSize({ width: 1280, height: 900 });
    await page.goto("/");

    await openWizard(page);
    const recent = page.getByRole("button").filter({ hasText: "/tmp/example" }).first();
    await recent.waitFor({ state: "visible", timeout: 5000 });
    await recent.click();
    await page.getByRole("button", { name: "Next" }).click();
    await expect(page.getByText("Name your session")).toBeVisible();
    await page.getByRole("button", { name: "Next" }).click();
    await expect(page.getByText("Which AI agent?")).toBeVisible();
    await page.getByRole("button", { name: /^codex/i }).click();
    await page.getByRole("button", { name: "Next" }).click();
    await expect(page.getByRole("heading", { name: "Review & Launch" })).toBeVisible();
    await page.getByRole("button", { name: /Launch session/ }).click();

    await expect.poll(() => captured.body?.tool).toBe("codex");
    expect(await page.evaluate(() => localStorage.getItem("aoe-acp-last-tool"))).toBe("codex");

    await page.reload();

    // Reopen via the keyboard shortcut (no prefill), so buildInitialData
    // picks the persisted tool up from localStorage. Walk back to the
    // Agent step and assert the codex tile carries the selected styling
    // (border-brand-600 is applied only when data.tool matches).
    await openWizard(page);
    await page.getByRole("button").filter({ hasText: "/tmp/example" }).first().click();
    await page.getByRole("button", { name: "Next" }).click();
    await expect(page.getByText("Name your session")).toBeVisible();
    await page.getByRole("button", { name: "Next" }).click();
    await expect(page.getByText("Which AI agent?")).toBeVisible();
    await expect(page.getByRole("button", { name: /^codex/i })).toHaveClass(/border-brand-600/);
  });

  test("Cmd/Ctrl+Enter on the Review step fires the create-session POST", async ({ page }) => {
    // ReviewStep registers a window-level keydown handler for
    // Enter + (metaKey || ctrlKey), so the chord submits without
    // touching the Launch button. Ported from the deleted live
    // wizard-launch-cmd-enter story (the click path stays live in
    // wizard-launch-button).
    const captured: { body: Record<string, unknown> | null } = { body: null };
    await mockApis(page, { captured });
    await page.setViewportSize({ width: 1280, height: 900 });
    await page.goto("/");
    await openReviewStep(page, "kbd-launch");

    await page.keyboard.press("ControlOrMeta+Enter");

    await expect.poll(() => captured.body?.tool).toBe("claude");
    expect(captured.body?.path).toBe("/tmp/example");
    expect(captured.body?.worktree_branch).toBe("kbd-launch");
  });
});
