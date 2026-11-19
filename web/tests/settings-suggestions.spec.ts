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
      show_source_ref: true,
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
  const github = page.getByLabel("Source GitHub");
  await github.focus();
  await page.keyboard.press("Alt+ArrowDown");
  await expect.poll(() => bodies).toContainEqual({ provider_order: ["linear", "github"] });
  await expect(page.getByLabel(/^Source /).first()).toHaveAccessibleName("Source Linear");
});

test("375px shows an arrow button instead of the drag handle", async ({ page }) => {
  await page.setViewportSize({ width: 375, height: 800 });
  const bodies = await setup(page);
  await page.goto("/settings/suggestions");
  await expect(page.getByRole("button", { name: "Drag GitHub to reorder" })).toBeHidden();
  await page.getByRole("button", { name: "Move Linear up" }).click();
  await expect.poll(() => bodies).toContainEqual({ provider_order: ["linear", "github"] });
});

test("a kind's preset saves through PUT /presets", async ({ page }) => {
  await setup(page);
  const puts: unknown[] = [];
  const preset = {
    agent: "claude",
    view: "structured",
    worktree: true,
    sandbox: false,
    yolo: false,
    prompt: "/aoe-review {pr-number}",
  };
  await page.route("**/api/suggestions/presets", (r) => {
    if (r.request().method() === "GET")
      return r.fulfill({
        json: {
          presets: [
            { provider_id: "github", criterion: "review-requested", customized: false, preset, defaults: preset },
          ],
        },
      });
    puts.push(r.request().postDataJSON());
    return r.fulfill({ json: { ok: true } });
  });
  await page.route(
    (url) => url.pathname === "/api/agents",
    (r) => r.fulfill({ json: [{ name: "claude", display_name: "Claude Code", kind: "builtin", installed: true }] }),
  );
  await page.goto("/settings/suggestions");
  await page.getByRole("button", { name: "Edit Review requests" }).click();
  await page.getByRole("switch", { name: "Sandbox" }).click();
  await page.getByRole("button", { name: "Save" }).click();
  await expect
    .poll(() => puts)
    .toEqual([
      expect.objectContaining({
        provider_id: "github",
        criterion: "review-requested",
        preset: expect.objectContaining({ sandbox: true }),
      }),
    ]);
});

test("a provider's suggestion_ plugin settings save through the settings PATCH", async ({ page }) => {
  await setup(page);
  const descriptor = (field: string, label: string) => ({
    section: "plugin:github",
    field,
    category: "Plugins",
    label,
    description: "",
    widget: { kind: "text" },
    web_write: { policy: "allow" },
    profile_overridable: false,
    validation: { rule: "none" },
    advanced: true,
    default: "",
  });
  const schema = [
    descriptor("suggestion_ignore", "Ignored GitHub suggestion sources"),
    descriptor("token_path", "Token path"),
  ];
  const patches: unknown[] = [];
  await page.route(
    (url) => url.pathname === "/api/settings/schema",
    (r) => r.fulfill({ json: schema }),
  );
  await page.route(
    (url) => url.pathname === "/api/settings",
    (r) => {
      if (r.request().method() === "PATCH") patches.push(r.request().postDataJSON());
      return r.fulfill({ json: { plugins: { github: { settings: {} } } } });
    },
  );
  await page.goto("/settings/suggestions");

  const section = page.getByRole("region", { name: "GitHub source settings" });
  await expect(section.getByText("Ignored GitHub suggestion sources")).toBeVisible();
  await expect(section.getByText("Token path")).toHaveCount(0);
  const input = section.getByRole("textbox");
  await input.fill("acme, acme/widgets");
  await input.blur();
  await expect.poll(() => patches).toContainEqual({ "plugin:github": { suggestion_ignore: "acme, acme/widgets" } });
});
