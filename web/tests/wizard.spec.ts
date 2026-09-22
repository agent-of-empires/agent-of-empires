// Single-screen new-session wizard (#2210): essentials, More options, agents, and the create payload.

import { test, expect } from "./helpers/mockedTest";
import {
  CLAUDE_AGENT,
  expandMoreOptions,
  launch,
  mockWizardApis,
  openWizard,
  selectAgent,
  selectProject,
  setTitle,
  startWizard,
  wizard,
} from "./helpers/wizard";

const WORKTREE_ON = { settings: { worktree: { enabled: true } } };
const BRANCH_PLACEHOLDER = "Uses session title if empty";
const GROUP_PLACEHOLDER = "Optional, for organizing related sessions";

test.describe("essentials", () => {
  test("recent project + default agent launches with one click, no paging", async ({ page }) => {
    const created = await startWizard(page, WORKTREE_ON);
    await expect(page.getByRole("button", { name: "Next" })).toHaveCount(0);
    await launch(page);
    await expect.poll(() => created[0]?.path).toBe("/tmp/example");
    expect(created[0]?.tool).toBe("claude");
  });

  test("only essentials show on open; advanced controls hide behind collapsed More options", async ({ page }) => {
    await startWizard(page, WORKTREE_ON);
    const w = wizard(page);
    await expect(w.getByRole("button", { name: /^claude/ })).toBeVisible();
    await expect(w.getByPlaceholder("Auto-generated if empty")).toHaveValue("");
    await expect(w.getByRole("button", { name: /Launch session/ })).toBeVisible();
    await expect(w.getByRole("button", { name: "More options" })).toHaveAttribute("aria-expanded", "false");
    for (const hidden of [
      w.getByRole("switch", { name: "Use structured view" }),
      w.getByText("Create a worktree"),
      w.getByText("Run in a safe container"),
      w.getByPlaceholder(BRANCH_PLACEHOLDER),
      w.getByPlaceholder(GROUP_PLACEHOLDER),
      w.getByRole("button", { name: "Base branch" }),
    ]) {
      await expect(hidden).toHaveCount(0);
    }

    await expandMoreOptions(page);
    await expect(w.getByRole("switch", { name: "Use structured view" })).toBeVisible();
    await expect(w.getByText("Run in a safe container")).toBeVisible();
    await expect(w.getByRole("switch", { name: /Create a worktree/ })).toHaveAttribute("aria-checked", "true");
    await expect(w.getByPlaceholder(BRANCH_PLACEHOLDER)).toBeVisible();
    await expect(w.getByRole("switch", { name: /Attach to existing branch/ })).toBeVisible();
    await expect(w.getByRole("button", { name: "Base branch" })).toBeVisible();
    const group = w.getByPlaceholder(GROUP_PLACEHOLDER);
    await group.fill("backend");
    await expect(group).toHaveValue("backend");
  });

  test("default_new_session_view = terminal opens the switch unticked and creates a terminal (#3517)", async ({
    page,
  }) => {
    const created = await startWizard(page, { settings: { acp: { default_new_session_view: "terminal" } } });
    await expandMoreOptions(page);
    await expect(wizard(page).getByRole("switch", { name: "Use structured view" })).toHaveAttribute(
      "aria-checked",
      "false",
    );
    await launch(page);
    await expect.poll(() => created[0]?.view).toBe("terminal");
  });

  test("Launch button shows the submitting state while the create POST is in flight", async ({ page }) => {
    let release!: () => void;
    const held = new Promise<void>((resolve) => (release = resolve));
    await startWizard(page, {
      onCreate: async (_body, route) => {
        await held;
        await route.fulfill({ json: { session: { id: "new-session" } } });
        return true;
      },
    });
    const launchBtn = page.getByRole("button", { name: /Launch session/ });
    await expect(launchBtn).toBeEnabled();
    await launchBtn.click();
    await expect(page.getByText("Creating session...")).toBeVisible();
    release();
  });

  test("Cmd/Ctrl+Enter fires the create-session POST with the default worktree", async ({ page }) => {
    const created = await startWizard(page, WORKTREE_ON);
    await setTitle(page, "kbd-launch");
    await page.keyboard.press("ControlOrMeta+Enter");
    await expect.poll(() => created[0]?.tool).toBe("claude");
    expect(created[0]?.path).toBe("/tmp/example");
    expect(created[0]?.worktree_enabled).toBe(true);
    // The branch derives server-side from the title unless edited.
    expect(created[0]?.worktree_branch).toBeUndefined();
    expect(created[0]?.create_new_branch).toBe(true);
  });

  test("group-level New session button prefills the wizard with the repo path", async ({ page }) => {
    await startWizard(page, { project: false });
    const groupHeader = page.locator('[data-testid="sidebar-group-header"]').first();
    await expect(groupHeader).toBeVisible();
    await groupHeader.getByRole("button", { name: /New session in /i }).click();
    await expect(page.getByRole("heading", { name: "New session" })).toBeVisible();
    // Scoped: the sidebar row behind the modal shows the same path.
    await expect(wizard(page).getByText("/tmp/example")).toBeVisible();
    await expect(wizard(page).getByRole("button", { name: /Launch session/ })).toBeEnabled();
  });

  test("wizard overlay outranks the z-50 tooltip layer on mobile", async ({ page }) => {
    // Tooltips portal a fixed z-50 span to body; an equal-z overlay loses to the later DOM node.
    await mockWizardApis(page, { sessions: [] });
    await page.setViewportSize({ width: 390, height: 844 });
    await page.goto("/");
    await openWizard(page);
    const overlayZ = await wizard(page).evaluate((el) => Number(getComputedStyle(el.parentElement!).zIndex));
    expect(overlayZ).toBeGreaterThan(50);
  });
});

test.describe("worktree and branch", () => {
  test("worktree toggle follows worktree.enabled and gates the branch input + Base branch section", async ({
    page,
  }) => {
    for (const enabled of [true, false]) {
      await page.unrouteAll({ behavior: "ignoreErrors" });
      await startWizard(page, { settings: { worktree: { enabled } } });
      await expandMoreOptions(page);
      const w = wizard(page);
      const toggle = w.getByRole("switch", { name: /Create a worktree/ });
      const controls = [w.getByPlaceholder(BRANCH_PLACEHOLDER), w.getByRole("button", { name: "Base branch" })];
      await expect(toggle).toHaveAttribute("aria-checked", String(enabled));
      for (const c of controls) await expect(c).toHaveCount(enabled ? 1 : 0);
      if (!enabled) continue;
      await toggle.click();
      await expect(toggle).toHaveAttribute("aria-checked", "false");
      for (const c of controls) await expect(c).toHaveCount(0);
      await toggle.click();
      await expect(toggle).toHaveAttribute("aria-checked", "true");
      await expect(controls[0]!).toBeVisible();
    }
  });

  test("typing a title derives the worktree branch while it is not manually edited", async ({ page }) => {
    await startWizard(page, WORKTREE_ON);
    await setTitle(page, "My Cool Feature");
    await expandMoreOptions(page);
    await expect(wizard(page).getByPlaceholder(BRANCH_PLACEHOLDER)).toHaveValue("my-cool-feature");
  });

  test("worktree branch and group set under More options flow into the create payload", async ({ page }) => {
    const created = await startWizard(page, WORKTREE_ON);
    await expandMoreOptions(page);
    await wizard(page).getByPlaceholder(BRANCH_PLACEHOLDER).fill("my-feature-branch");
    await wizard(page).getByPlaceholder(GROUP_PLACEHOLDER).fill("backend");
    await launch(page);
    await expect.poll(() => created[0]?.worktree_enabled).toBe(true);
    expect(created[0]).toMatchObject({
      worktree_branch: "my-feature-branch",
      create_new_branch: true,
      group: "backend",
    });
  });

  // #969: attaching to an existing branch sends create_new_branch false and hides the new-branch base picker.
  test("attach to existing branch hides Base branch and sends create_new_branch=false", async ({ page }) => {
    const created = await startWizard(page, WORKTREE_ON);
    await expandMoreOptions(page);
    const w = wizard(page);
    const attach = w.getByRole("switch", { name: /Attach to existing branch/ });
    await expect(attach).toHaveAttribute("aria-checked", "false");
    await w.getByPlaceholder(BRANCH_PLACEHOLDER).fill("feat/existing");
    await attach.click();
    await expect(attach).toHaveAttribute("aria-checked", "true");
    await expect(w.getByRole("button", { name: "Base branch" })).toHaveCount(0);
    await launch(page);
    await expect.poll(() => created[0]?.create_new_branch).toBe(false);
    expect(created[0]?.base_branch).toBeUndefined();
  });

  // #948
  test("Base branch is collapsed, fetches remote branches on expand, and fills from a pick", async ({ page }) => {
    const branchUrls: URL[] = [];
    await page.route("**/api/git/branches**", (r) => {
      branchUrls.push(new URL(r.request().url()));
      return r.fulfill({
        json: [
          { name: "main", is_current: true },
          { name: "feature/x", is_current: false },
          { name: "release-1.2", is_current: false, remote_only: true },
        ],
      });
    });
    await startWizard(page, WORKTREE_ON);
    await expandMoreOptions(page);
    const w = wizard(page);
    await expect(w.getByLabel("Base branch")).toHaveCount(0);
    await w.getByRole("button", { name: "Base branch" }).click();
    const baseInput = w.getByLabel("Base branch");
    await expect(baseInput).toBeVisible();
    await expect.poll(() => branchUrls.at(-1)?.searchParams.get("include_remote")).toBe("true");
    await baseInput.click();
    const option = w.getByRole("option", { name: /release-1\.2/ });
    await expect(option).toBeVisible();
    await option.click();
    await expect(baseInput).toHaveValue("release-1.2");
  });
});

test.describe("agents and presets", () => {
  test("agent picker renders installed agents, including terminal fallback tools", async ({ page }) => {
    await startWizard(page, {
      agents: ["claude", "codex", "antigravity"]
        .map((name) => ({ ...CLAUDE_AGENT, name, binary: name }))
        .concat([{ ...CLAUDE_AGENT, name: "uninstalled-tool", installed: false, install_hint: "brew install x" }]),
    });
    const w = wizard(page);
    for (const name of ["claude", "codex", "antigravity"]) {
      await expect(w.getByRole("button", { name, exact: true })).toBeVisible();
    }
    await expect(w.getByRole("button", { name: "uninstalled-tool", exact: true })).toHaveCount(0);
  });

  test("wizard remembers the last-picked agent across reloads", async ({ page }) => {
    // A non-default tool, so a broken restore cannot pass via the claude fallback.
    const created = await startWizard(page, {
      agents: [CLAUDE_AGENT, { ...CLAUDE_AGENT, name: "codex", binary: "codex" }],
    });
    await selectAgent(page, /^codex/i);
    await launch(page);
    await expect.poll(() => created[0]?.tool).toBe("codex");
    // Saved after the create response, which lands after the request is captured.
    await expect.poll(() => page.evaluate(() => localStorage.getItem("aoe-acp-last-tool"))).toBe("codex");
    await page.reload();
    await openWizard(page);
    await selectProject(page, "/tmp/example");
    await expect(page.getByRole("button", { name: /^codex/i })).toHaveClass(/border-brand-600/);
  });

  test("profile picker is hidden when there is only one profile", async ({ page }) => {
    await startWizard(page, { profiles: [{ name: "default", is_default: true }] });
    await expandMoreOptions(page);
    await expect(wizard(page).getByText("Workflow preset")).toHaveCount(0);
  });

  test("profile picker shows descriptions and selecting one applies its sandbox + yolo defaults", async ({ page }) => {
    const settingsProfiles: string[] = [];
    page.on("request", (req) => {
      if (new URL(req.url()).pathname === "/api/settings") {
        settingsProfiles.push(new URL(req.url()).searchParams.get("profile") ?? "");
      }
    });
    await startWizard(page, {
      docker: true,
      profiles: [
        { name: "default", is_default: true, description: "Stock setup with no overrides" },
        { name: "yolo-sandbox", is_default: false, description: "Auto-approve in a container" },
        { name: "no-desc", is_default: false },
      ],
      profileSettings: {
        "yolo-sandbox": {
          sandbox: { enabled_by_default: true, environment: ["FOO=bar"] },
          session: { yolo_mode_default: true, default_tool: "claude" },
        },
      },
    });
    await expandMoreOptions(page);
    const w = wizard(page);
    await expect(w.getByText("Workflow preset")).toBeVisible();
    // #949: descriptions render under names; profiles without one still appear.
    await expect(w.getByText("Stock setup with no overrides")).toBeVisible();
    await expect(w.getByText("Auto-approve in a container")).toBeVisible();
    await expect(w.getByRole("radio", { name: /no-desc/ })).toBeVisible();

    await w.getByRole("radio", { name: /yolo-sandbox/ }).click();
    for (const label of ["Run in a safe container", "Auto-approve actions"]) {
      await expect(w.locator("label", { hasText: label }).locator("role=switch")).toHaveAttribute(
        "aria-checked",
        "true",
      );
    }
    expect(settingsProfiles).toContain("yolo-sandbox");
  });

  test("sandbox toggle is disabled when Docker is not running", async ({ page }) => {
    await startWizard(page);
    await expandMoreOptions(page);
    const w = wizard(page);
    await expect(w.locator("label", { hasText: "Run in a safe container" }).locator("role=switch")).toBeDisabled();
    await expect(w.getByText("Docker is not running.")).toBeVisible();
  });

  test("extra args propagate to the create-session POST body", async ({ page }) => {
    const created = await startWizard(page);
    await expandMoreOptions(page);
    await wizard(page).getByPlaceholder("e.g. --port 8080").fill("--verbose");
    await launch(page);
    await expect.poll(() => created[0]?.extra_args).toBe("--verbose");
  });

  test("shows and launches a configured custom agent without exposing sensitive fields", async ({ page }) => {
    const hidden = [
      "/opt/private/bin/remote-helper",
      "ssh prod.example.com remote-helper",
      "agent_detect_as",
      "shell string",
    ];
    const expectHidden = async (texts: string[]) => {
      for (const text of texts) await expect(page.locator("body")).not.toContainText(text);
    };
    const created = await startWizard(page, {
      agents: [
        { ...CLAUDE_AGENT, kind: "builtin", installed: false, install_hint: "install claude" },
        { ...CLAUDE_AGENT, name: "remote-helper", kind: "custom", binary: hidden[0] },
      ],
    });
    const w = wizard(page);
    await expect(w.getByText("No agents installed")).toHaveCount(0);
    await expect(w.getByRole("button", { name: /remote-helper/ })).toContainText("Custom");
    // Anchored: the recent-project row's label also mentions the seed's claude tool.
    await expect(w.getByRole("button", { name: /^claude/ })).toHaveCount(0);

    await selectAgent(page, /remote-helper/);
    await expectHidden(hidden);
    // Without agent_acp_cmd a custom agent is terminal-only; the override preview may show its binary by design.
    await expandMoreOptions(page);
    await expect(w.getByText(/Custom agents run in the terminal unless they define agent_acp_cmd/)).toBeVisible();
    await expectHidden(hidden.slice(1));
    await launch(page);
    await expect.poll(() => created[0]?.tool).toBe("remote-helper");
    expect(created[0]?.view === "structured").toBe(false);
    await expectHidden(hidden.slice(0, 3));
  });
});

test.describe("confirmations before create", () => {
  // #2066: repo on_create hooks need approval; the server refuses with hooks_need_trust until trust_hooks is sent.
  test("hooks-trust modal lists the commands; Cancel aborts and Proceed resubmits with trust", async ({ page }) => {
    const created = await startWizard(page, {
      onCreate: (body, route) =>
        body.trust_hooks === true
          ? undefined
          : route
              .fulfill({
                status: 403,
                json: {
                  error: "hooks_need_trust",
                  message: "Repository hooks require trust. Resubmit with trust_hooks: true to approve.",
                  on_create: ["bash scripts/setup-worktree.sh", "cp .env.example .env"],
                  on_launch: ["npm run dev-seed"],
                  on_destroy: [],
                  needs_mcp_trust: false,
                },
              })
              .then(() => true),
    });
    const dialog = page.getByTestId("hooks-trust-dialog");
    const trusted = () => created.filter((b) => b.trust_hooks === true).length;

    await launch(page);
    await expect(dialog).toBeVisible();
    // Approval trusts the whole hooks hash, so on_launch is listed too.
    for (const cmd of ["bash scripts/setup-worktree.sh", "cp .env.example .env", "npm run dev-seed"]) {
      await expect(dialog).toContainText(cmd);
    }
    expect(created).toHaveLength(1);
    await page.getByRole("button", { name: "Cancel" }).click();
    await expect(dialog).toHaveCount(0);
    expect(trusted()).toBe(0);
    await expect(wizard(page).getByRole("button", { name: /Launch session/ })).toBeEnabled();

    await launch(page);
    await expect(dialog).toBeVisible();
    await page.getByTestId("hooks-trust-proceed").click();
    await expect.poll(trusted).toBe(1);
  });

  // #2045: glob volume_ignores expand once at create time, so a sandbox create confirms the snapshot first.
  test.describe("glob volume_ignores", () => {
    async function launchSandbox(page: import("@playwright/test").Page) {
      const acknowledged: number[] = [];
      await page.route("**/api/sandbox/volume-ignores-preview**", (r) =>
        r.fulfill({
          json: {
            acknowledged: false,
            globs: [
              {
                pattern: "**/bin",
                matched_paths: ["/workspace/example/src/App/bin", "/workspace/example/tests/Lib/bin"],
              },
              { pattern: "**/obj", matched_paths: ["/workspace/example/src/App/obj"] },
            ],
          },
        }),
      );
      await page.route("**/api/app-state/volume-ignores-globs-acknowledged", (r) => {
        if (r.request().method() === "POST") acknowledged.push(1);
        return r.fulfill({ json: { has_acknowledged_volume_ignores_globs: true } });
      });
      const created = await startWizard(page, { docker: true });
      await expandMoreOptions(page);
      const sandbox = wizard(page).locator("label", { hasText: "Run in a safe container" }).locator("role=switch");
      await sandbox.click();
      await expect(sandbox).toHaveAttribute("aria-checked", "true");
      await launch(page);
      const dialog = page.getByTestId("volume-ignores-glob-dialog");
      await expect(dialog).toBeVisible();
      return { created, acknowledged, dialog };
    }

    test("modal shows the patterns and match count; Cancel aborts the create", async ({ page }) => {
      const { created, acknowledged, dialog } = await launchSandbox(page);
      for (const text of ["**/bin", "**/obj", "3 directories"]) await expect(dialog).toContainText(text);
      expect(created).toHaveLength(0);
      await page.getByRole("button", { name: "Cancel" }).click();
      await expect(dialog).toHaveCount(0);
      expect(created).toHaveLength(0);
      expect(acknowledged).toHaveLength(0);
      await expect(wizard(page).getByRole("button", { name: /Launch session/ })).toBeEnabled();
    });

    for (const dontShowAgain of [false, true]) {
      test(`Proceed ${dontShowAgain ? "with" : "without"} 'Don't show again' creates the session`, async ({ page }) => {
        const { created, acknowledged } = await launchSandbox(page);
        if (dontShowAgain) await page.getByTestId("volume-ignores-glob-dont-show-again").click();
        await page.getByTestId("volume-ignores-glob-proceed").click();
        await expect.poll(() => created.length).toBe(1);
        await expect.poll(() => acknowledged.length).toBe(dontShowAgain ? 1 : 0);
      });
    }
  });
});
