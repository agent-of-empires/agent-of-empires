// Full-text settings search (#2213): typing in the settings search box filters
// the schema-backed settings to a hit list, and selecting a hit jumps to that
// field's tab and scrolls/highlights the field, opening the Advanced fold when
// the target lives inside it. The RTL suites pin the index and the box in
// isolation; this spec is the real-DOM cross-tab jump.

import { test, expect } from "./helpers/mockedTest";
import type { Page } from "@playwright/test";

const ALLOW = { policy: "allow" };
const NONE = { rule: "none" };

const SCHEMA = [
  {
    section: "sandbox",
    field: "enabled_by_default",
    label: "Enabled by Default",
    widget: { kind: "toggle" },
    advanced: false,
  },
  {
    section: "acp",
    field: "show_tool_durations",
    label: "Show tool-call durations",
    widget: { kind: "toggle" },
    advanced: false,
  },
  {
    section: "acp",
    field: "silent_orphan_grace_secs",
    label: "Silent-orphan grace (s)",
    widget: { kind: "number", min: 0 },
    advanced: true,
  },
].map((d) => ({
  category: d.section,
  description: "",
  profile_overridable: true,
  validation: NONE,
  web_write: ALLOW,
  ...d,
}));

async function installMocks(page: Page) {
  await page.route(
    (url) => url.pathname === "/api/sessions",
    (r) => r.fulfill({ json: { sessions: [], workspace_ordering: [] } }),
  );
  await page.route(
    (url) => url.pathname === "/api/about",
    (r) => r.fulfill({ json: { read_only: false, auth_mode: "none", behind_tunnel: false, profile: "main" } }),
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
    (r) => r.fulfill({ json: { sandbox: {}, acp: {} } }),
  );
  await page.route(
    (url) => /^\/api\/profiles\/[^/]+\/settings$/.test(url.pathname),
    (route) => route.fulfill({ json: { ok: true } }),
  );
}

test("search jumps to a primary field on another tab and highlights it", async ({ page }) => {
  await installMocks(page);
  await page.goto("/settings/sandbox");

  await expect(page.getByText("Enabled by Default")).toBeVisible();

  await page.getByPlaceholder("Search settings...").fill("durations");

  // The matching hit is listed; the unrelated sandbox field is filtered out.
  const hit = page.getByTestId("settings-search-hit-acp-show_tool_durations");
  await expect(hit).toBeVisible();
  await expect(page.getByTestId("settings-search-hit-sandbox-enabled_by_default")).toHaveCount(0);

  await hit.click();

  // Jumped to the Structured view tab and the field is scrolled in + flashed.
  await expect(page.getByRole("heading", { name: "Structured view" })).toBeVisible();
  const target = page.locator('[data-settings-field="acp.show_tool_durations"]');
  await expect(target).toBeVisible();
  await expect(target).toHaveClass(/animate-settings-highlight/);
});

test("search opens the Advanced fold when the target field lives inside it", async ({ page }) => {
  await installMocks(page);
  await page.goto("/settings/sandbox");

  await page.getByPlaceholder("Search settings...").fill("orphan");
  await page.getByTestId("settings-search-hit-acp-silent_orphan_grace_secs").click();

  // The advanced field is visible without manually expanding the fold.
  await expect(page.getByRole("heading", { name: "Structured view" })).toBeVisible();
  await expect(page.locator('[data-settings-field="acp.silent_orphan_grace_secs"]')).toBeVisible();
});
