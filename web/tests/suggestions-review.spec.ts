import { test, expect } from "./helpers/mockedTest";
import { mockTerminalApis } from "./helpers/terminal-mocks";

const item = {
  id: "issue-1",
  provider_id: "github",
  title: "Fix session lifecycle",
  reason: "Assigned to you",
  source_url: "https://example.test/issues/1",
  source_ref: "example/1",
  preview: "UNTRUSTED PROVIDER BODY",
  project: "aoe",
  criterion: "assigned",
  draft: { title: "Fix session lifecycle", prompt: "Review the issue and implement it." },
  dismissed: false,
};

const provider = {
  id: "github",
  name: "GitHub",
  status: "ok",
  enabled: true,
  sources: [
    { criterion: "assigned", label: "Assigned issues", description: "", enabled: true, variables: [] },
    { criterion: "ci", label: "Failing checks", description: "", enabled: false, variables: [] },
  ],
};

// main requires a runtime cursor on session responses; without it the client marks the daemon offline.
const CURSOR = { "aoe-runtime-epoch": "mock-runtime", "aoe-runtime-revision": "1" };

test("Start opens the wizard with the draft prompt and creates nothing until Launch", async ({ page }) => {
  await mockTerminalApis(page);
  const sessionPosts: string[] = [];
  await page.route("**/api/sessions", (route) => {
    if (route.request().method() === "POST") {
      sessionPosts.push(route.request().postData() ?? "");
      return route.fulfill({ status: 400, json: { message: "Boundary create refusal" } });
    }
    return route.fulfill({ headers: CURSOR, json: { sessions: [], workspace_ordering: [] } });
  });
  await page.route("**/api/suggestions", (route) => route.fulfill({ json: { providers: [provider], items: [item] } }));
  await page.route("**/api/suggestions/prepare", (route) =>
    route.fulfill({
      json: {
        title: item.draft.title,
        initial_prompt: item.draft.prompt,
        source_ref: item.source_ref,
        project: { name: "aoe", path: "/work/aoe" },
        tool: "claude",
        yolo: false,
        sandbox: false,
        worktree: false,
      },
    }),
  );
  await page.route("**/api/settings**", (route) => route.fulfill({ json: {} }));
  await page.route("**/api/projects**", (route) => route.fulfill({ json: [] }));
  await page.route("**/api/git/is-repo**", (route) => route.fulfill({ json: { is_git_repo: true } }));
  await page.goto("/suggestions");

  await expect(page.getByRole("heading", { name: "Suggested sessions" })).toBeVisible();
  await expect(page.getByText("GitHub · Assigned to you · aoe")).toBeVisible();
  await expect(page.getByText("UNTRUSTED PROVIDER BODY")).toHaveCount(0);

  await page.getByRole("button", { name: "Start" }).click();
  const prompt = page.getByLabel("Prompt", { exact: true });
  await expect(prompt).toHaveValue(item.draft.prompt);
  await expect(prompt).not.toHaveValue(/UNTRUSTED/);
  await expect(page.getByLabel("Title")).toHaveValue(item.draft.title);
  const from = page.getByTestId("wizard-source");
  await expect(from).toContainText("GitHub");
  await expect(from).toContainText(item.title);
  await expect(from).toContainText("issue text not included");
  await expect(from.getByRole("link", { name: /Open/ })).toHaveAttribute("href", item.source_url);
  expect(sessionPosts).toEqual([]);

  const create = page.waitForRequest((r) => r.url().endsWith("/api/sessions") && r.method() === "POST");
  await page.getByRole("button", { name: /Launch/ }).click();
  expect((await create).postDataJSON()).toMatchObject({ initial_prompt: item.draft.prompt, path: "/work/aoe" });
});

test("dismissed items move to the dismissed list and can be restored", async ({ page }) => {
  await mockTerminalApis(page);
  await page.route("**/api/sessions", (route) =>
    route.fulfill({ headers: CURSOR, json: { sessions: [], workspace_ordering: [] } }),
  );
  const live = { providers: [provider], items: [{ ...item }] };
  await page.route("**/api/suggestions", (route) => route.fulfill({ json: live }));
  await page.route("**/api/suggestions/dismiss", (route) => {
    live.items[0].dismissed = true;
    return route.fulfill({ json: { ok: true } });
  });
  await page.route("**/api/suggestions/undismiss", (route) => {
    live.items[0].dismissed = false;
    return route.fulfill({ json: { ok: true } });
  });
  await page.goto("/suggestions");

  await page.getByRole("button", { name: "Dismiss", exact: true }).click();
  await expect(page.getByText(item.title, { exact: true })).toHaveCount(0);
  await page.getByRole("button", { name: "Dismissed (1)" }).click();
  await page.getByRole("button", { name: "Restore" }).click();
  await page.getByRole("button", { name: "Show active" }).click();
  await expect(page.getByText(item.title, { exact: true })).toBeVisible();
});

test("a provider error stays visible next to the last items", async ({ page }) => {
  await mockTerminalApis(page);
  await page.route("**/api/sessions", (route) =>
    route.fulfill({ headers: CURSOR, json: { sessions: [], workspace_ordering: [] } }),
  );
  await page.route("**/api/suggestions", (route) =>
    route.fulfill({
      json: { providers: [{ ...provider, status: "error", error: "Rate limit exceeded" }], items: [item] },
    }),
  );
  await page.goto("/suggestions");

  await expect(page.getByRole("region", { name: "Provider GitHub" })).toContainText("Rate limit exceeded");
  await expect(page.getByText(item.title, { exact: true })).toBeVisible();
});

test("read-only dashboard disables Start, hides Dismiss and says why", async ({ page }) => {
  await mockTerminalApis(page);
  await page.route("**/api/about", (route) =>
    route.fulfill({ json: { read_only: true, auth_mode: "none", behind_tunnel: false, profile: "main" } }),
  );
  await page.route("**/api/sessions", (route) =>
    route.fulfill({ headers: CURSOR, json: { sessions: [], workspace_ordering: [] } }),
  );
  await page.route("**/api/suggestions", (route) => route.fulfill({ json: { providers: [provider], items: [item] } }));
  await page.goto("/suggestions");

  const start = page.getByRole("button", { name: "Start" });
  await expect(start).toBeDisabled();
  await expect(start).toHaveAttribute("title", "Read-only: can't create Sessions");
  await expect(page.getByRole("button", { name: "Dismiss" })).toHaveCount(0);
  await expect(page.getByText("This dashboard is read-only. Start, Dismiss and Refresh are off.")).toBeVisible();
});

test("an empty list says there is nothing to suggest", async ({ page }) => {
  await mockTerminalApis(page);
  await page.route("**/api/sessions", (route) =>
    route.fulfill({ headers: CURSOR, json: { sessions: [], workspace_ordering: [] } }),
  );
  await page.route("**/api/suggestions", (route) => route.fulfill({ json: { providers: [provider], items: [] } }));
  await page.goto("/suggestions");

  await expect(page.getByText("Nothing to suggest right now.")).toBeVisible();
});
