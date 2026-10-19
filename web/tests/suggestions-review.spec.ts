import { test, expect } from "./helpers/mockedTest";
import { mockTerminalApis } from "./helpers/terminal-mocks";

const suggestion = {
  id: "issue-4562",
  title: "Fix session lifecycle",
  reason: "A source-backed task",
  source_url: "https://example.test/issues/4562",
  source_ref: "https://example.test/issues/4562#comment-2",
  preview: "Add the shared flow",
  project: "aoe",
  criterion: "assigned-to-me",
  draft: { title: "Fix session lifecycle", prompt: "Review issue and implement the requested flow." },
  provider_id: "github",
  provider_name: "GitHub",
  revision: 7,
  dismissed: false,
  session_id: null,
  project_path: "/work/aoe",
  can_start: true,
  start_blocked_reason: null,
};

const snapshot = {
  items: [suggestion],
  providers: [
    { provider_id: "github", provider_name: "GitHub", status: "ok", error: null, updated_at: "2026-10-18T12:00:00Z" },
  ],
  auto_create: { enabled: false, rules: [], last_error: null },
};

test("suggestions remain available without sessions and support mobile keyboard review", async ({ page }) => {
  await mockTerminalApis(page);
  await page.route("**/api/sessions", (route) => route.fulfill({ json: { sessions: [], workspace_ordering: [] } }));
  await page.route("**/api/suggestions**", (route) => route.fulfill({ json: snapshot }));
  await page.route("**/api/suggestions/prepare**", (route) =>
    route.fulfill({
      json: {
        project_path: suggestion.project_path,
        title: suggestion.draft.title,
        prompt: suggestion.draft.prompt,
        source_ref: suggestion.source_ref,
        suggestion: { provider_id: suggestion.provider_id, id: suggestion.id, revision: suggestion.revision },
      },
    }),
  );
  await page.route("**/api/settings**", (route) =>
    route.fulfill({
      json: {
        session: { default_tool: "opencode", yolo_mode_default: false },
        acp: { acp_defaults: { opencode: { model: "openai/gpt-5.5", effort: "high" } } },
        worktree: { enabled: true },
      },
    }),
  );
  await page.route("**/api/projects**", (route) => route.fulfill({ json: [] }));
  await page.route("**/api/git/is-repo**", (route) => route.fulfill({ json: { is_git_repo: true } }));
  await page.setViewportSize({ width: 375, height: 812 });
  await page.goto("/suggestions?profile=work");

  await expect(page.getByRole("heading", { name: "Suggested sessions" })).toBeVisible();
  await expect(page.getByRole("heading", { name: "Fix session lifecycle" })).toBeVisible();
  await expect(page.getByText("Automation policy")).toBeVisible();
  await expect(page.getByRole("button", { name: "Start" })).toBeVisible();
  await expect(page.getByRole("link", { name: /View source/ })).toHaveAttribute("href", suggestion.source_url);

  const promptSummary = page.getByText("Actual draft prompt");
  await expect(page.getByText(suggestion.draft.prompt)).toBeVisible();
  await promptSummary.focus();
  await page.keyboard.press("Enter");
  await expect(page.getByText(suggestion.draft.prompt)).not.toBeVisible();
  await page.keyboard.press("Enter");
  await expect(page.getByText(suggestion.draft.prompt)).toBeVisible();

  await page.getByRole("button", { name: "Customize" }).click();
  const prompt = page.getByLabel("Draft task prompt");
  await expect(prompt).toHaveValue(suggestion.draft.prompt);
  await prompt.fill("Reviewed prompt from the browser\n");
  await page.route("**/api/sessions", async (route) => {
    if (route.request().method() !== "POST") {
      await route.fallback();
      return;
    }
    await route.fulfill({ status: 400, json: { message: "Boundary create refusal" } });
  });
  const create = page.waitForRequest(
    (request) => request.url().endsWith("/api/sessions") && request.method() === "POST",
  );
  await page.getByRole("button", { name: /Launch session/ }).click();
  expect((await create).postDataJSON()).toMatchObject({
    path: suggestion.project_path,
    title: suggestion.draft.title,
    prompt: "Reviewed prompt from the browser\n",
    suggestion: { provider_id: suggestion.provider_id, id: suggestion.id, revision: suggestion.revision },
    source_refs: [suggestion.source_ref],
    profile: "work",
    tool: "opencode",
    agent_model: "openai/gpt-5.5",
    agent_effort: "high",
    yolo_mode: false,
    worktree_enabled: true,
  });
  await expect(page.getByText("Boundary create refusal")).toBeVisible();
});

test("suggestions can be dismissed, restored, and started through the browser flow", async ({ page }) => {
  await mockTerminalApis(page);
  await page.route("**/api/sessions", (route) => route.fulfill({ json: { sessions: [], workspace_ordering: [] } }));
  const liveSnapshot = structuredClone(snapshot);
  await page.route("**/api/suggestions**", async (route) => {
    if (route.request().method() === "GET") {
      await route.fulfill({ json: liveSnapshot });
      return;
    }
    await route.fallback();
  });
  await page.route("**/api/suggestions/action**", async (route) => {
    const action = route.request().postDataJSON() as { action: string };
    if (action.action === "dismiss") liveSnapshot.items[0].dismissed = true;
    if (action.action === "restore") liveSnapshot.items[0].dismissed = false;
    await route.fulfill({
      json: action.action === "start" ? { session_id: "session-123", created: true } : { ok: true },
    });
  });
  await page.goto("/suggestions?profile=work");

  await expect(page.getByRole("heading", { name: suggestion.title })).toBeVisible();
  await page.getByRole("button", { name: "Dismiss", exact: true }).click();
  await expect(page.getByRole("heading", { name: suggestion.title })).not.toBeVisible();
  await page.getByRole("button", { name: "Dismissed (1)" }).click();
  await page.getByRole("button", { name: "Restore" }).click();
  await expect(page.getByRole("heading", { name: suggestion.title })).not.toBeVisible();
  await page.getByRole("button", { name: "Show active" }).click();
  await expect(page.getByRole("button", { name: "Start" })).toBeVisible();
  await page.getByRole("button", { name: "Start" }).click();

  await expect(page).toHaveURL(/\/session\/session-123$/);
});

test("provider errors stay visible alongside the last successful suggestions", async ({ page }) => {
  await mockTerminalApis(page);
  await page.route("**/api/sessions", (route) => route.fulfill({ json: { sessions: [], workspace_ordering: [] } }));
  const failedSnapshot = structuredClone(snapshot);
  failedSnapshot.providers[0] = {
    ...failedSnapshot.providers[0],
    status: "error",
    error: "Rate limit exceeded",
  };
  await page.route("**/api/suggestions**", (route) => route.fulfill({ json: failedSnapshot }));
  await page.goto("/suggestions?profile=work");

  await expect(page.getByRole("region", { name: "Provider status" })).toContainText("Rate limit exceeded");
  await expect(page.getByRole("region", { name: "Provider status" })).toContainText(
    "Previously collected items remain available.",
  );
  await expect(page.getByRole("heading", { name: suggestion.title })).toBeVisible();
  await expect(page.getByRole("heading", { name: "No suggestions yet" })).toHaveCount(0);
});

test("opens suggestions from the mobile sidebar without losing the active profile", async ({ page }) => {
  await mockTerminalApis(page);
  await page.route("**/api/sessions", (route) => route.fulfill({ json: { sessions: [], workspace_ordering: [] } }));
  await page.route("**/api/suggestions**", (route) => route.fulfill({ json: snapshot }));
  await page.setViewportSize({ width: 375, height: 812 });
  await page.goto("/?profile=work");

  await page.getByRole("button", { name: "Toggle sidebar" }).click();
  await page.getByRole("button", { name: "Suggested sessions" }).click();

  await expect(page).toHaveURL(/\/suggestions\?profile=work$/);
  await expect(page.getByRole("heading", { name: suggestion.title })).toBeVisible();
});

test("opens suggestions from the desktop sidebar without hiding the sidebar", async ({ page }) => {
  await mockTerminalApis(page);
  await page.route("**/api/sessions", (route) => route.fulfill({ json: { sessions: [], workspace_ordering: [] } }));
  await page.route("**/api/suggestions**", (route) => route.fulfill({ json: snapshot }));
  await page.setViewportSize({ width: 1280, height: 900 });
  await page.goto("/?profile=work");

  await page.getByRole("button", { name: "Suggested sessions" }).click();

  await expect(page).toHaveURL(/\/suggestions\?profile=work$/);
  await expect(page.getByRole("heading", { name: suggestion.title })).toBeVisible();
  await expect(page.getByRole("button", { name: "Suggested sessions" })).toBeVisible();
});
