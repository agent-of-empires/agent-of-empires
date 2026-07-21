// Story #3 (#1515): expanding an "Advanced" fold and editing a knob inside it
// persists through the same save-on-change path as any other field. Ported
// from live to the mocked suite: a canned schema (mirroring the real
// `#[setting(...)]` labels) plus a stateful settings store stand in for the
// backend, so the reload assertion exercises the same fetch-render-expand
// path against the value the PATCH wrote.
//
// The RTL fold suite (SettingsView.folds.test.tsx) pins the hide/expand/save
// logic per section; this spec keeps the real-DOM pass: URL-routed tabs, the
// folded-by-default markup after a genuine page load, and the PATCH wire
// format of an advanced edit.

import { test, expect } from "./helpers/mockedTest";
import type { Page } from "@playwright/test";

const ALLOW = { policy: "allow" };
const ELEV = { policy: "requires_elevation", reason: "host isolation" };
const NONE = { rule: "none" };

// Representative slice of the real schema: one primary anchor + one advanced
// field per folded tab, with labels matching the real `#[setting(label)]`
// values so the selectors stay honest.
const SCHEMA = [
  {
    section: "sandbox",
    field: "enabled_by_default",
    label: "Enabled by Default",
    widget: { kind: "toggle" },
    advanced: false,
    web_write: ELEV,
  },
  {
    section: "sandbox",
    field: "cpu_limit",
    label: "CPU Limit",
    widget: { kind: "optional_text" },
    advanced: true,
    web_write: ELEV,
  },
  {
    section: "worktree",
    field: "enabled",
    label: "Enabled by Default",
    widget: { kind: "toggle" },
    advanced: false,
    web_write: ELEV,
  },
  {
    section: "worktree",
    field: "bare_repo_path_template",
    label: "Bare Repo Template",
    widget: { kind: "text" },
    advanced: true,
    web_write: ELEV,
  },
  {
    section: "acp",
    field: "show_tool_durations",
    label: "Show tool-call durations",
    widget: { kind: "toggle" },
    advanced: false,
    web_write: ALLOW,
  },
  {
    section: "acp",
    field: "silent_orphan_grace_secs",
    label: "Silent-orphan grace (s)",
    widget: { kind: "number", min: 0 },
    advanced: true,
    web_write: ALLOW,
  },
  {
    section: "logging",
    field: "default_level",
    label: "Default level",
    widget: {
      kind: "select",
      options: ["trace", "debug", "info", "warn", "error"].map((v) => ({ value: v, label: v })),
    },
    advanced: false,
    web_write: ALLOW,
  },
  {
    section: "logging",
    field: "output",
    label: "Output (restart req.)",
    widget: {
      kind: "select",
      options: [
        { value: "file", label: "file" },
        { value: "stdout", label: "stdout" },
      ],
    },
    advanced: true,
    web_write: ALLOW,
  },
].map((d) => ({
  category: d.section,
  description: "",
  profile_overridable: true,
  validation: NONE,
  ...d,
}));

interface FoldMockHandle {
  /** Stateful per-section settings store; PATCHes merge into it so a reload
   *  reads the written value back. */
  settings: Record<string, Record<string, unknown>>;
  patches: Array<Record<string, unknown>>;
}

async function installFoldMocks(page: Page): Promise<FoldMockHandle> {
  const handle: FoldMockHandle = {
    settings: { sandbox: {}, worktree: {}, acp: {}, logging: {} },
    patches: [],
  };

  await page.route(
    (url) => url.pathname === "/api/sessions",
    (r) => r.fulfill({ json: { sessions: [], workspace_ordering: [] } }),
  );
  await page.route(
    (url) => url.pathname === "/api/about",
    (r) =>
      r.fulfill({
        json: { read_only: false, auth_mode: "none", behind_tunnel: false, profile: "main" },
      }),
  );
  await page.route(
    (url) => url.pathname === "/api/profiles",
    (r) => r.fulfill({ json: [{ name: "main", is_default: true }] }),
  );
  await page.route(
    (url) => url.pathname === "/api/settings/schema",
    (r) => r.fulfill({ json: SCHEMA }),
  );
  await page.route(
    (url) => url.pathname === "/api/settings",
    (r) => r.fulfill({ json: handle.settings }),
  );
  await page.route(
    (url) => /^\/api\/profiles\/[^/]+\/settings$/.test(url.pathname),
    (route) => {
      if (route.request().method() !== "PATCH") return route.fulfill({ json: handle.settings });
      const body = route.request().postDataJSON() as Record<string, Record<string, unknown>>;
      handle.patches.push(body);
      for (const [section, fields] of Object.entries(body)) {
        handle.settings[section] = { ...handle.settings[section], ...fields };
      }
      return route.fulfill({ json: { ok: true } });
    },
  );

  return handle;
}

function fieldByLabel(page: Page, label: RegExp) {
  return page.locator("label", { hasText: label });
}

test("sandbox advanced knob edits persist after expanding the fold", async ({ page }) => {
  const handle = await installFoldMocks(page);

  await page.goto("/settings/sandbox");

  // A high-level control is visible immediately; the advanced knob is folded
  // away by default.
  await expect(page.getByText("Enabled by Default")).toBeVisible();
  await expect(fieldByLabel(page, /^CPU Limit$/)).toHaveCount(0);

  await page
    .getByRole("button", { name: /Advanced/ })
    .first()
    .click();

  const cpuInput = fieldByLabel(page, /^CPU Limit$/)
    .locator("..")
    .locator('input[type="text"]');
  await expect(cpuInput).toBeVisible();

  // Edit and commit (TextField commits on blur / Enter).
  await cpuInput.fill("4");
  await cpuInput.press("Enter");

  // The PATCH carries exactly the edited leaf.
  await expect.poll(() => handle.patches).toEqual([{ sandbox: { cpu_limit: "4" } }]);
  expect(handle.settings.sandbox.cpu_limit).toBe("4");

  // After reload the fold is collapsed again (component-local, not
  // persisted), and re-expanding shows the value the store handed back.
  await page.reload();
  await expect(page.getByText("Enabled by Default")).toBeVisible();
  await expect(fieldByLabel(page, /^CPU Limit$/)).toHaveCount(0);

  await page
    .getByRole("button", { name: /Advanced/ })
    .first()
    .click();
  await expect(cpuInput).toHaveValue("4");
});

// The other three folded tabs (Worktree, Structured view, Logging) each render
// their advanced fields only once the fold is expanded. Drive each one in the
// browser so the relocated field markup is exercised through real URL routing.
test("worktree, structured-view, and logging advanced folds expand in the browser", async ({ page }) => {
  await installFoldMocks(page);

  const cases: Array<{ tab: string; anchor: string; field: RegExp }> = [
    { tab: "worktree", anchor: "Enabled by Default", field: /^Bare Repo Template$/ },
    { tab: "structured-view", anchor: "Show tool-call durations", field: /^Silent-orphan grace \(s\)$/ },
    { tab: "logging", anchor: "Default level", field: /^Output \(restart req\.\)$/ },
  ];

  for (const { tab, anchor, field } of cases) {
    await page.goto(`/settings/${tab}`);
    await expect(page.getByText(anchor).first()).toBeVisible();

    // Folded away by default.
    await expect(fieldByLabel(page, field)).toHaveCount(0);

    await page
      .getByRole("button", { name: /Advanced/ })
      .first()
      .click();
    await expect(fieldByLabel(page, field)).toBeVisible();
  }
});
