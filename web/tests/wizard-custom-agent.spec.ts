import { test, expect, Page } from "@playwright/test";

const customAgentName = "remote-helper";
const hiddenBinary = "/opt/private/bin/remote-helper";
const hiddenCommand = "ssh prod.example.com remote-helper";
const hiddenDetectAs = "agent_detect_as";

async function mockWizardApis(page: Page, agents: unknown[]) {
  await page.route("**/api/login/status", (route) => route.fulfill({ json: { required: false, authenticated: true } }));
  await page.route("**/api/settings", (route) => route.fulfill({ json: {} }));
  await page.route("**/api/themes", (route) => route.fulfill({ json: [] }));
  await page.route("**/api/profiles", (route) => route.fulfill({ json: [] }));
  await page.route("**/api/groups", (route) => route.fulfill({ json: [] }));
  await page.route("**/api/devices", (route) => route.fulfill({ json: [] }));
  await page.route("**/api/system/update-status", (route) => route.fulfill({ json: {} }));
  await page.route("**/api/about", (route) => route.fulfill({ json: {} }));
  await page.route("**/api/docker/status", (route) => route.fulfill({ json: { available: false, runtime: null } }));
  await page.route("**/api/agents", (route) => route.fulfill({ json: agents }));
  await page.route("**/api/sessions", (route) => {
    if (route.request().method() === "GET") {
      return route.fulfill({
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
    return route.fulfill({ json: { session: { id: "new-session" } } });
  });
}

async function openWizardOnAgentStep(page: Page) {
  await page.locator("body").click();
  await page.keyboard.press("n");
  await expect(page.getByRole("heading", { name: "New session" })).toBeVisible();
  const recent = page.getByRole("button").filter({ hasText: "/tmp/example" }).first();
  await recent.waitFor({ state: "visible", timeout: 5000 });
  await recent.click();
  const next = page.getByRole("button", { name: "Next" });
  await expect(next).toBeEnabled();
  await next.click();
  await expect(page.getByText("Name your session")).toBeVisible();
  await next.click();
  await expect(page.getByText("Which AI agent?")).toBeVisible();
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
      return route.fulfill({
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
    await openWizardOnAgentStep(page);

    await expect(page.getByText("No agents installed")).toHaveCount(0);
    await expect(page.getByRole("button", { name: /remote-helper/ })).toBeVisible();
    await expect(page.getByRole("button", { name: /remote-helper/ })).toContainText("Custom");
    await expect(page.getByRole("button", { name: /claude/ })).toHaveCount(0);

    await page.getByRole("button", { name: /remote-helper/ }).click();
    await expect(page.getByText(/Custom agents run in the terminal unless they define agent_acp_cmd/)).toBeVisible();
    await expect(page.locator("body")).not.toContainText(hiddenBinary);
    await expect(page.locator("body")).not.toContainText(hiddenCommand);
    await expect(page.locator("body")).not.toContainText(hiddenDetectAs);
    await expect(page.locator("body")).not.toContainText("shell string");

    await page.getByRole("button", { name: "Next" }).click();
    await expect(page.getByText("Review & Launch")).toBeVisible();
    const agentRow = page.getByRole("button", { name: /Agent/ });
    await expect(agentRow).toContainText(customAgentName);
    await expect(agentRow).toContainText("Custom");
    await agentRow.click();
    await expect(page.getByText("Which AI agent?")).toBeVisible();
    await page.getByRole("button", { name: "Next" }).click();
    await page.getByRole("button", { name: /Launch session/ }).click();

    await expect.poll(() => captured?.tool).toBe(customAgentName);
    expect(captured?.view === "structured").toBe(false);
    await expect(page.locator("body")).not.toContainText(hiddenBinary);
    await expect(page.locator("body")).not.toContainText(hiddenCommand);
    await expect(page.locator("body")).not.toContainText(hiddenDetectAs);
  });
});
