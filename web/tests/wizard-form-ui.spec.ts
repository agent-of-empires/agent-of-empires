import { test, expect } from "./helpers/mockedTest";
import { Page } from "@playwright/test";
import { openWizard, selectProject, selectAgent, expandMoreOptions, setTitle, launch, wizard } from "./helpers/wizard";

// Wizard form UI stories on the single-screen wizard (#2210). Covers:
// - branch auto-derivation from the title (the reducer slugifies the title
//   into the worktree branch input while it is undirtied);
// - the group-level "New session in <group>" sidebar button prefilling
//   the wizard with the repo path;
// - last-picked agent persistence across reloads via the
//   "aoe-acp-last-tool" localStorage key (#1133 / #1135);
// - Cmd/Ctrl+Enter submitting the create-session POST (LaunchFooter's
//   window-level keydown handler).

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
        // worktree.enabled drives the wizard's "Create a worktree" default (#2423);
        // the branch input only renders while worktree is on.
        json:
          path === "settings"
            ? { worktree: { enabled: true } }
            : path === "about" || path === "system/update-status"
              ? {}
              : [],
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

test.describe("Wizard form UI stories", () => {
  test("typing a title derives the worktree branch in the branch input", async ({ page }) => {
    // SET_FIELD title slugifies into worktreeBranch while it is undirtied
    // (wizardReducer.ts). On the single screen the branch input lives in
    // the worktree controls under More options.
    await mockApis(page);
    await page.setViewportSize({ width: 1280, height: 900 });
    await page.goto("/");
    await openWizard(page);
    await selectProject(page, "/tmp/example");
    await setTitle(page, "autogen branch here");
    // The branch input lives in the worktree controls under More options.
    await expandMoreOptions(page);

    await expect(page.getByPlaceholder("Uses session title if empty")).toHaveValue("autogen-branch-here");
  });

  test("group-level New session button prefills the wizard with the repo path", async ({ page }) => {
    // WorkspaceSidebar group headers render a per-group "New session in
    // <group>" button. Clicking it routes through App.tsx's
    // handleCreateSession, which sets wizardPrefill { path } so the wizard
    // opens with the repo path already selected.
    await mockApis(page);
    await page.setViewportSize({ width: 1280, height: 900 });
    await page.goto("/");

    const groupHeader = page.locator('[data-testid="sidebar-group-header"]').first();
    await expect(groupHeader).toBeVisible();
    await groupHeader.getByRole("button", { name: /New session in /i }).click();

    const w = wizard(page);
    await expect(page.getByRole("heading", { name: "New session" })).toBeVisible();
    // Scope to the modal: the seeded sidebar row behind it also shows this path.
    await expect(w.getByText("/tmp/example")).toBeVisible();
    // Path prefilled => the launch gate is satisfied immediately.
    await expect(w.getByRole("button", { name: /Launch session/ })).toBeEnabled();
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
    await selectProject(page, "/tmp/example");
    await selectAgent(page, /^codex/i);
    await launch(page);

    await expect.poll(() => captured.body?.tool).toBe("codex");
    // saveLastUsedTool runs after the create RESPONSE resolves, while the
    // poll above passes as soon as the REQUEST is captured by the route, so
    // an immediate read races the response handling. Poll until persisted.
    await expect.poll(() => page.evaluate(() => localStorage.getItem("aoe-acp-last-tool"))).toBe("codex");

    await page.reload();

    // Reopen via the keyboard shortcut (no prefill), so buildInitialData
    // picks the persisted tool up from localStorage. The codex tile must
    // carry the selected styling (border-brand-600 applies only when
    // data.tool matches).
    await openWizard(page);
    await selectProject(page, "/tmp/example");
    await expect(page.getByRole("button", { name: /^codex/i })).toHaveClass(/border-brand-600/);
  });

  test("Cmd/Ctrl+Enter fires the create-session POST", async ({ page }) => {
    // LaunchFooter registers a window-level keydown handler for
    // Enter + (metaKey || ctrlKey), so the chord submits without touching
    // the Launch button.
    const captured: { body: Record<string, unknown> | null } = { body: null };
    await mockApis(page, { captured });
    await page.setViewportSize({ width: 1280, height: 900 });
    await page.goto("/");
    await openWizard(page);
    await selectProject(page, "/tmp/example");
    await setTitle(page, "kbd-launch");

    await page.keyboard.press("ControlOrMeta+Enter");

    await expect.poll(() => captured.body?.tool).toBe("claude");
    expect(captured.body?.path).toBe("/tmp/example");
    expect(captured.body?.worktree_enabled).toBe(true);
    expect(captured.body?.worktree_branch).toBeUndefined();
  });
});
