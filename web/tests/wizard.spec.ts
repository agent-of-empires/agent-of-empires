// New-session wizard: TUI-style rows, detail panels, agents, progress, and the create payload.

import { test, expect } from "./helpers/mockedTest";
import {
  CLAUDE_AGENT,
  closePanel,
  launch,
  mockWizardApis,
  openPanel,
  openWizard,
  selectAgent,
  selectProject,
  setTitle,
  sessionStub,
  startWizard,
  wizard,
} from "./helpers/wizard";

const WORKTREE_ON = { settings: { worktree: { enabled: true } } };
const BRANCH_PLACEHOLDER = "Uses session title if empty";
const GROUP_LABEL = "Group";
const switchNamed = (page: import("@playwright/test").Page, name: string) => wizard(page).getByRole("switch", { name });

test.describe("essentials", () => {
  test("the form shows one row per field; worktree details open in a panel", async ({ page }) => {
    await startWizard(page, WORKTREE_ON);
    const w = wizard(page);
    await expect(w.getByTestId("wizard-project-row")).toContainText("/tmp/example");
    await expect(w.getByPlaceholder("Auto-generated if empty")).toHaveValue("");
    await expect(w.getByTestId("wizard-agent-row")).toContainText("claude");
    await expect(switchNamed(page, "Use structured view")).toHaveAttribute("aria-checked", "true");
    await expect(switchNamed(page, "Auto-approve actions")).toHaveAttribute("aria-checked", "false");
    await expect(switchNamed(page, "Create a worktree")).toHaveAttribute("aria-checked", "true");
    await expect(w.getByTestId("wizard-worktree-row")).toContainText("auto, new branch");
    await expect(w.getByRole("button", { name: /Launch session/ })).toBeVisible();
    await expect(w.getByPlaceholder(BRANCH_PLACEHOLDER)).toHaveCount(0);

    await openPanel(page, "Worktree");
    await expect(w.getByPlaceholder(BRANCH_PLACEHOLDER)).toBeVisible();
    await expect(w.getByRole("switch", { name: /Attach to existing branch/ })).toBeVisible();
    await expect(w.getByLabel("Base branch")).toBeVisible();
    await expect(w.getByRole("button", { name: /Launch session/ })).toHaveCount(0);
    await w.getByPlaceholder(BRANCH_PLACEHOLDER).fill("feat/rows");
    await closePanel(page);
    await expect(w.getByTestId("wizard-worktree-row")).toContainText("feat/rows, new branch");

    const group = w.getByLabel(GROUP_LABEL);
    await group.fill("backend");
    await expect(group).toHaveValue("backend");
  });

  test("default_new_session_view = terminal opens the switch unticked and creates a terminal (#3517)", async ({
    page,
  }) => {
    const created = await startWizard(page, { settings: { acp: { default_new_session_view: "terminal" } } });
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
    // The recent project and default agent need no paging.
    await expect(page.getByRole("button", { name: "Next" })).toHaveCount(0);
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
    await expect(wizard(page).getByTestId("wizard-project-row")).toContainText("/tmp/example");
    await expect(wizard(page).getByRole("button", { name: /Launch session/ })).toBeEnabled();
    // Like the TUI, a known project skips straight to the title.
    await expect(wizard(page).getByPlaceholder("Auto-generated if empty")).toBeFocused();
  });

  test("wizard overlay outranks the z-50 tooltip layer on mobile", async ({ page }) => {
    // Tooltips portal a fixed z-50 span to body; an equal-z overlay loses to the later DOM node.
    await mockWizardApis(page, { sessions: [] });
    await page.setViewportSize({ width: 390, height: 844 });
    await page.goto("/");
    await openWizard(page);
    // Paint order, not the z-index value: a z-[60] inside the shell's fixed layers still
    // ranks below a body-level z-50 tooltip.
    const tooltipOnTop = await wizard(page).evaluate((sheet) => {
      const tip = document.createElement("span");
      tip.className = "fixed z-50";
      const r = sheet.getBoundingClientRect();
      Object.assign(tip.style, {
        left: `${r.left + 10}px`,
        top: `${r.top + r.height / 2}px`,
        width: "40px",
        height: "20px",
      });
      document.body.appendChild(tip);
      const hit = document.elementFromPoint(r.left + 20, r.top + r.height / 2 + 10);
      tip.remove();
      return hit === tip;
    });
    expect(tooltipOnTop).toBe(false);
  });
});

test.describe("create progress", () => {
  test("a slow create shows the running hook and its output, and can continue in the background", async ({ page }) => {
    let release!: () => void;
    const held = new Promise<void>((resolve) => (release = resolve));
    const polledKeys: string[] = [];
    await page.route("**/api/sessions/create-progress/**", (r) => {
      polledKeys.push(decodeURIComponent(new URL(r.request().url()).pathname.split("/").pop()!));
      return r.fulfill({
        json: { stage: "running_hooks", hook: "npm install", output: ["resolving deps", "added 12 packages"] },
      });
    });
    const created = await startWizard(page, {
      onCreate: async (_body, route) => {
        await held;
        await route.fulfill({ json: sessionStub({ id: "slow-session", title: "slow-one" }) });
        return true;
      },
    });
    await launch(page);
    const w = wizard(page);
    await expect(w.getByTestId("create-progress-hook")).toContainText("npm install");
    await expect(w.getByTestId("create-progress-output")).toContainText("added 12 packages");
    expect(polledKeys[0]).toBe(created[0]?.idempotency_key);

    await w.getByRole("button", { name: "Continue in background" }).click();
    await expect(w).toHaveCount(0);
    release();
    await expect(page.getByText('"slow-one" is ready')).toBeVisible();
  });
});

test.describe("mobile", () => {
  test.use({ hasTouch: true, isMobile: true, viewport: { width: 390, height: 844 } });

  test("the wizard is a bottom sheet sized to its rows, with Launch right under them", async ({ page }) => {
    await mockWizardApis(page);
    await page.goto("/");
    // The `n` shortcut is live once the dashboard has loaded its sessions.
    await expect(page.getByTestId("sidebar-session-row").first()).toBeVisible();
    await openWizard(page);
    await selectProject(page, "/tmp/example");
    const w = wizard(page);
    const viewport = page.viewportSize()!;
    const box = (await w.boundingBox())!;
    expect(Math.round(box.y + box.height)).toBe(viewport.height);
    expect(box.height).toBeLessThan(viewport.height * 0.9);
    const launchBtn = w.getByRole("button", { name: /Launch session/ });
    await expect(launchBtn).toBeInViewport();
    await expect(launchBtn).not.toContainText("Enter");
    // No dead band between the last row and Launch.
    const group = (await w.getByLabel("Group").boundingBox())!;
    const launch = (await launchBtn.boundingBox())!;
    expect(launch.y - (group.y + group.height)).toBeLessThan(80);
  });

  test("a long panel grows the sheet and scrolls instead of overflowing", async ({ page }) => {
    await mockWizardApis(page, {
      projects: Array.from({ length: 30 }, (_, i) => ({ name: `proj-${i}`, path: `/tmp/proj-${i}`, scope: "global" })),
    });
    await page.goto("/");
    await expect(page.getByTestId("sidebar-session-row").first()).toBeVisible();
    await openWizard(page);
    const box = (await wizard(page).boundingBox())!;
    expect(box.y).toBeGreaterThanOrEqual(0);
    await expect(wizard(page).getByRole("button", { name: "Done" })).toBeInViewport();
  });
});

test.describe("worktree and branch", () => {
  // #969: attaching to an existing branch sends create_new_branch false and hides the new-branch base picker.
  test("attach to existing branch hides Base branch; branch, group and extra args reach the create payload", async ({
    page,
  }) => {
    const created = await startWizard(page, WORKTREE_ON);
    await openPanel(page, "Worktree");
    const w = wizard(page);
    const attach = w.getByRole("switch", { name: /Attach to existing branch/ });
    await expect(attach).toHaveAttribute("aria-checked", "false");
    await w.getByPlaceholder(BRANCH_PLACEHOLDER).fill("feat/existing");
    await attach.click();
    await expect(attach).toHaveAttribute("aria-checked", "true");
    await expect(w.getByLabel("Base branch")).toHaveCount(0);
    await closePanel(page);
    // Fields from the form and the agent panel ride along in the same payload.
    await w.getByLabel(GROUP_LABEL).fill("backend");
    await openPanel(page, "Agent");
    await w.getByPlaceholder("e.g. --port 8080").fill("--verbose");
    await closePanel(page);
    await launch(page);
    await expect.poll(() => created[0]?.create_new_branch).toBe(false);
    expect(created[0]).toMatchObject({
      worktree_enabled: true,
      worktree_branch: "feat/existing",
      group: "backend",
      extra_args: "--verbose",
    });
    expect(created[0]?.base_branch).toBeUndefined();
  });

  // #948
  test("Base branch fetches remote branches and fills from a pick", async ({ page }) => {
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
    await openPanel(page, "Worktree");
    const w = wizard(page);
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
    // The launched project is remembered too, so the form opens directly.
    await expect(wizard(page).getByTestId("wizard-project-row")).toContainText("/tmp/example");
    await expect(wizard(page).getByTestId("wizard-agent-row")).toContainText("codex");
  });

  test("profile panel shows descriptions and selecting one applies its sandbox + yolo defaults", async ({ page }) => {
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
    await openPanel(page, "Profile");
    const w = wizard(page);
    await expect(w.getByText("Workflow preset")).toBeVisible();
    // #949: descriptions render under names; profiles without one still appear.
    await expect(w.getByText("Stock setup with no overrides")).toBeVisible();
    await expect(w.getByText("Auto-approve in a container")).toBeVisible();
    await expect(w.getByRole("radio", { name: /no-desc/ })).toBeVisible();

    await w.getByRole("radio", { name: /yolo-sandbox/ }).click();
    // A pick returns to the form.
    for (const label of ["Run in a safe container", "Auto-approve actions"]) {
      await expect(switchNamed(page, label)).toHaveAttribute("aria-checked", "true");
    }
    expect(settingsProfiles).toContain("yolo-sandbox");
  });

  test("sandbox toggle is disabled when Docker is not running", async ({ page }) => {
    await startWizard(page);
    await expect(switchNamed(page, "Run in a safe container")).toBeDisabled();
    await expect(wizard(page).getByTestId("wizard-sandbox-row")).toContainText("Docker is not running");
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
    await openPanel(page, "Agent");
    await expect(w.getByText("No agents installed")).toHaveCount(0);
    await expect(w.getByRole("button", { name: /remote-helper/ })).toContainText("Custom");
    // Anchored: the recent-project row's label also mentions the seed's claude tool.
    await expect(w.getByRole("button", { name: /^claude/ })).toHaveCount(0);
    await closePanel(page);

    await selectAgent(page, /remote-helper/);
    await expectHidden(hidden);
    // Without agent_acp_cmd a custom agent is terminal-only; the override preview may show its binary by design.
    await expect(switchNamed(page, "Use structured view")).toBeDisabled();
    await expect(w.getByText(/needs agent_acp_cmd/)).toBeVisible();
    await openPanel(page, "Agent");
    await expectHidden(hidden.slice(1));
    await closePanel(page);
    await launch(page);
    await expect.poll(() => created[0]?.tool).toBe("remote-helper");
    expect(created[0]?.view === "structured").toBe(false);
    await expectHidden(hidden.slice(0, 3));
  });
});

test.describe("confirmations before create", () => {
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
      const sandbox = switchNamed(page, "Run in a safe container");
      await sandbox.click();
      await expect(sandbox).toHaveAttribute("aria-checked", "true");
      await launch(page);
      const dialog = page.getByTestId("volume-ignores-glob-dialog");
      await expect(dialog).toBeVisible();
      return { created, acknowledged, dialog };
    }

    test("modal shows the patterns and match count; Cancel aborts, Proceed with Don't show again creates", async ({
      page,
    }) => {
      const { created, acknowledged, dialog } = await launchSandbox(page);
      for (const text of ["**/bin", "**/obj", "3 directories"]) await expect(dialog).toContainText(text);
      expect(created).toHaveLength(0);
      await page.getByRole("button", { name: "Cancel" }).click();
      await expect(dialog).toHaveCount(0);
      expect(created).toHaveLength(0);
      expect(acknowledged).toHaveLength(0);

      await launch(page);
      await expect(dialog).toBeVisible();
      await page.getByTestId("volume-ignores-glob-dont-show-again").click();
      await page.getByTestId("volume-ignores-glob-proceed").click();
      await expect.poll(() => created.length).toBe(1);
      await expect.poll(() => acknowledged.length).toBe(1);
    });
  });
});
