import { test, expect } from "./helpers/mockedTest";
import { mockSettingsApis } from "./helpers/apiMocks";
import { mockTerminalApis } from "./helpers/terminal-mocks";

const source = (criterion: string, label: string, enabled: boolean, variables: string[]) => ({
  criterion,
  label,
  description: "",
  enabled,
  variables,
});

function store() {
  return {
    prefs: {
      homepage: true,
      sidebar: false,
      refresh_minutes: 15,
      start_without_review: false,
      provider_order: ["github", "linear"],
    },
    providers: [
      {
        id: "github",
        name: "GitHub",
        status: "ok",
        enabled: true,
        updated_at: Math.floor(Date.now() / 1000) - 120,
        sources: [
          source("review-requested", "Review requests", true, [
            "pr-number",
            "repo",
            "org",
            "branch",
            "commit-sha",
            "who-requested",
            "timestamp",
          ]),
          source("assigned-issue", "Assigned issues", true, ["issue-number", "repo"]),
          source("ci-failed", "CI failures", false, ["branch", "commit-sha"]),
        ],
      },
      {
        id: "linear",
        name: "Linear",
        status: "error",
        error: "API key rejected.",
        enabled: true,
        sources: [source("assigned", "Assigned issues", true, ["ticket"])],
      },
    ],
    items: [
      {
        id: "1",
        provider_id: "github",
        title: "Review: fix lifecycle",
        reason: "Review requested",
        source_url: "",
        source_ref: "o/r#4562",
        preview: "",
        criterion: "review-requested",
        mapping_key: "github.com/agent-of-empires/agent-of-empires",
        variables: { "pr-number": "4562" },
        draft: { title: "t", prompt: "/aoe-review #4562" },
        dismissed: false,
      },
    ],
  };
}

async function setup(page: import("@playwright/test").Page) {
  const live = store();
  const prefsBodies: unknown[] = [];
  await mockTerminalApis(page);
  await mockSettingsApis(page);
  await page.route("**/api/suggestions", (r) => r.fulfill({ json: live }));
  await page.route("**/api/suggestions/prefs", (r) => {
    const body = r.request().postDataJSON() as { provider_order?: string[] };
    prefsBodies.push(body);
    if (body.provider_order) {
      live.prefs.provider_order = body.provider_order;
      live.providers.sort((a, b) => body.provider_order!.indexOf(a.id) - body.provider_order!.indexOf(b.id));
    }
    return r.fulfill({ json: { ok: true } });
  });
  await page.route("**/api/suggestions/templates", (r) =>
    r.fulfill({ json: r.request().method() === "GET" ? { templates: [] } : { ok: true } }),
  );
  await page.route("**/api/suggestions/mappings", (r) =>
    r.fulfill({ json: r.request().method() === "GET" ? { mappings: [] } : { ok: true } }),
  );
  await page.route("**/api/projects**", (r) =>
    r.fulfill({ json: [{ name: "agent-of-empires", path: "/work/aoe", scope: "global", pinned: false }] }),
  );
  return prefsBodies;
}

test("Alt+ArrowDown reorders the focused provider and persists the order", async ({ page }) => {
  const bodies = await setup(page);
  await page.goto("/settings/suggestions");
  const github = page.getByLabel("Provider GitHub");
  await github.focus();
  await page.keyboard.press("Alt+ArrowDown");
  await expect.poll(() => bodies).toContainEqual({ provider_order: ["linear", "github"] });
  await expect(page.getByLabel(/^Provider /).first()).toHaveAccessibleName("Provider Linear");
});

test("375px shows an arrow button instead of the drag handle", async ({ page }) => {
  await page.setViewportSize({ width: 375, height: 800 });
  const bodies = await setup(page);
  await page.goto("/settings/suggestions");
  await expect(page.getByRole("button", { name: "Drag GitHub to reorder" })).toBeHidden();
  await page.getByRole("button", { name: "Move Linear up" }).click();
  await expect.poll(() => bodies).toContainEqual({ provider_order: ["linear", "github"] });
});
