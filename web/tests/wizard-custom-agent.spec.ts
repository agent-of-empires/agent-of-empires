import { test, expect, Page } from "@playwright/test";
import { openWizard, selectProject, selectAgent, expandMoreOptions, launch, wizard } from "./helpers/wizard";

// Custom agent picker on the single-screen wizard (#2210). A configured
// custom agent shows in the always-visible picker with a "Custom" badge,
// launches with view=terminal (custom agents are not ACP-capable unless
// they define agent_acp_cmd), and never leaks its binary / command /
// detect-as into the UI.

const customAgentName = "remote-helper";
const hiddenBinary = "/opt/private/bin/remote-helper";
const hiddenCommand = "ssh prod.example.com remote-helper";
const hiddenDetectAs = "agent_detect_as";

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

async function mockWizardApis(page: Page, agents: unknown[]) {
  await page.route("**/api/login/status", (route) => route.fulfill({ json: { required: false, authenticated: true } }));
  await page.route("**/api/settings", (route) => route.fulfill({ json: {} }));
  await page.route("**/api/themes", (route) => route.fulfill({ json: [] }));
  await page.route("**/api/profiles", (route) => route.fulfill({ json: [] }));
  await page.route("**/api/groups", (route) => route.fulfill({ json: [] }));
  await page.route("**/api/devices", (route) => route.fulfill({ json: [] }));
  await page.route("**/api/system/update-status", (route) => route.fulfill({ json: {} }));
  await page.route("**/api/about", (route) => route.fulfill({ json: {} }));
  // ProjectStep always mounts now and fetches these.
  await page.route("**/api/recent-projects", (route) => route.fulfill({ json: { projects: [] } }));
  await page.route("**/api/projects", (route) => route.fulfill({ json: [] }));
  await page.route("**/api/docker/status", (route) => route.fulfill({ json: { available: false, runtime: null } }));
  await page.route("**/api/agents", (route) => route.fulfill({ json: agents }));
  await page.route("**/api/sessions", (route) => {
    if (route.request().method() === "GET") {
      return route.fulfill({ json: seedSessionsPayload() });
    }
    return route.fulfill({ json: { session: { id: "new-session" } } });
  });
}

test.describe("wizard custom agent picker", () => {
  test("shows and launches a configured custom agent without exposing sensitive fields", async ({ page }) => {
    let captured: { tool?: string; view?: "structured" | "terminal" } | null = null;
    await mockWizardApis(page, [
      {
        name: "claude",
        kind: "builtin",
        binary: "claude",
        host_only: false,
        installed: false,
        install_hint: "install claude",
      },
      {
        name: customAgentName,
        kind: "custom",
        binary: hiddenBinary,
        host_only: false,
        installed: true,
        install_hint: "",
      },
    ]);
    await page.route("**/api/sessions", (route) => {
      if (route.request().method() === "POST") {
        captured = JSON.parse(route.request().postData() || "{}");
        return route.fulfill({ json: { session: { id: "new-session" } } });
      }
      return route.fulfill({ json: seedSessionsPayload() });
    });

    await page.setViewportSize({ width: 1280, height: 900 });
    await page.goto("/");
    await openWizard(page);
    await selectProject(page, "/tmp/example");

    const w = wizard(page);
    // Picker is always visible. The not-installed builtin claude is filtered
    // out; the custom agent shows with a "Custom" badge.
    await expect(w.getByText("No agents installed")).toHaveCount(0);
    await expect(w.getByRole("button", { name: /remote-helper/ })).toBeVisible();
    await expect(w.getByRole("button", { name: /remote-helper/ })).toContainText("Custom");
    // Anchored so this matches only the (absent) agent-picker tile, not the
    // recent-project row whose label includes the seed session's "claude" tool.
    await expect(w.getByRole("button", { name: /^claude/ })).toHaveCount(0);

    await selectAgent(page, /remote-helper/);
    // Default (collapsed) view: the picker shows only the agent name, never
    // its binary / command / detect-as.
    await expect(page.locator("body")).not.toContainText(hiddenBinary);
    await expect(page.locator("body")).not.toContainText(hiddenCommand);
    await expect(page.locator("body")).not.toContainText(hiddenDetectAs);
    await expect(page.locator("body")).not.toContainText("shell string");

    // Custom agents that do not define agent_acp_cmd are terminal-only, so
    // the structured-view picker is replaced by the terminal fallback notice
    // under More options (#2210). The command-override preview there does
    // surface the resolved launch command (binary), which is by design.
    await expandMoreOptions(page);
    await expect(w.getByText(/Custom agents run in the terminal unless they define agent_acp_cmd/)).toBeVisible();
    await expect(page.locator("body")).not.toContainText(hiddenCommand);
    await expect(page.locator("body")).not.toContainText(hiddenDetectAs);
    await expect(page.locator("body")).not.toContainText("shell string");

    await launch(page);

    await expect.poll(() => captured?.tool).toBe(customAgentName);
    expect(captured?.view === "structured").toBe(false);
    await expect(page.locator("body")).not.toContainText(hiddenBinary);
    await expect(page.locator("body")).not.toContainText(hiddenCommand);
    await expect(page.locator("body")).not.toContainText(hiddenDetectAs);
  });
});
