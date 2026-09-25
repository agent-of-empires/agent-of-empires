// CityHall mode (#7) reduces Settings to curated tabs and the wizard to a title; the server derives the rest.
// Pane hiding and server lockdown are covered by unit tests.

import { test, expect } from "./helpers/mockedTest";
import { mockSettingsApis } from "./helpers/apiMocks";
import type { Page } from "@playwright/test";
import { openWizard, wizard } from "./helpers/wizard";

const SCHEMA = [
  {
    section: "theme",
    field: "name",
    label: "Theme",
    widget: { kind: "select", options: [{ value: "dark", label: "dark" }] },
  },
  { section: "theme", field: "color_mode", label: "Color mode", widget: { kind: "select", options: [] } },
  { section: "theme", field: "idle_decay_minutes", label: "Idle decay", widget: { kind: "number" } },
  { section: "session", field: "delete_to_trash", label: "Delete to Trash", widget: { kind: "toggle" } },
  { section: "session", field: "confirm_delete", label: "Confirm Before Delete", widget: { kind: "toggle" } },
  {
    section: "session",
    field: "trash_retention_days",
    label: "Trash Retention (days)",
    widget: { kind: "number" },
  },
  { section: "session", field: "idle_auto_stop", label: "Idle auto-stop", widget: { kind: "toggle" } },
  { section: "sandbox", field: "yolo_mode", label: "Yolo mode", widget: { kind: "toggle" } },
].map((d) => ({
  category: d.section,
  description: "",
  profile_overridable: true,
  validation: { rule: "none" },
  advanced: false,
  web_write: { policy: "allow" },
  ...d,
}));

async function installCityHallMocks(page: Page) {
  await mockSettingsApis(page, {
    about: () => ({ cityhall_mode: true }),
    schema: SCHEMA,
    settings: () => ({ theme: { name: "dark" }, session: { delete_to_trash: true, trash_retention_days: 30 } }),
  });
  await page.route(
    (url) => url.pathname === "/api/projects",
    (r) => r.fulfill({ json: [{ name: "app", path: "/repos/app", scope: "global", pinned: false }] }),
  );
  await page.route(
    (url) => url.pathname === "/api/recent-projects",
    (r) => r.fulfill({ json: { projects: [] } }),
  );
  await page.route(
    (url) => url.pathname === "/api/groups",
    (r) => r.fulfill({ json: [] }),
  );
  await page.route(
    (url) => url.pathname === "/api/docker/status",
    (r) => r.fulfill({ json: { available: false, runtime: null } }),
  );
  await page.route(
    (url) => url.pathname === "/api/agents",
    (r) =>
      r.fulfill({
        json: [
          { name: "claude", kind: "builtin", binary: "claude", host_only: false, installed: true, install_hint: "" },
        ],
      }),
  );
  // Shaped so mutating controls would render outside CityHall.
  await page.route(
    (url) => url.pathname === "/api/mcp/servers",
    (r) =>
      r.fulfill({
        json: {
          agent: "claude",
          effective: [],
          keptOnRemoval: [{ name: "old", transport: "stdio", provenance: "kept" }],
          conflicts: [{ name: "dup", agent: "claude", previous: "a", current: "b", fingerprint: "fp" }],
          driftPaused: false,
        },
      }),
  );
  await page.route(
    (url) => url.pathname === "/api/plugins",
    (r) =>
      r.fulfill({
        json: {
          plugins: [
            {
              id: "acme",
              name: "Acme",
              version: "1.0.0",
              description: "test plugin",
              icon: null,
              icon_asset_url: null,
              enabled: true,
              builtin: false,
              validation: "community",
              source: "gh:acme/acme",
              capabilities: [],
              ui_contributions: [],
              granted: true,
              needs_reapproval: false,
            },
          ],
          load_errors: [],
        },
      }),
  );
}

test("Settings, its tabs, and settings search are curated to the CityHall subset; MCP and Plugins are read-only", async ({
  page,
}) => {
  await installCityHallMocks(page);
  await page.goto("/settings");

  // Scope to the visible tab strip; desktop and mobile both render one.
  for (const label of ["Theme", "Sessions", "MCP servers", "Telemetry", "Plugins"]) {
    await expect(page.locator("button:visible", { hasText: label }).first()).toBeVisible();
  }
  await expect(page.getByText("Sandbox")).toHaveCount(0);
  await expect(page.getByText("Worktree")).toHaveCount(0);
  await expect(page.getByText("Security")).toHaveCount(0);
  await expect(page.getByText("Profiles")).toHaveCount(0);

  // Search must not reach uncurated fields either (a UX gate; the server 403s the write).
  const search = page.getByPlaceholder("Search settings...");
  await search.fill("yolo");
  await expect(page.getByTestId("settings-search-hit-sandbox-yolo_mode")).toHaveCount(0);
  await expect(page.getByText("No matching settings")).toBeVisible();
  await search.fill("idle");
  await expect(page.getByTestId("settings-search-hit-session-idle_auto_stop")).toHaveCount(0);
  await expect(page.getByTestId("settings-search-hit-theme-idle_decay_minutes")).toHaveCount(0);
  await search.fill("trash");
  await expect(page.getByTestId("settings-search-hit-session-delete_to_trash")).toBeVisible();

  await page.goto("/settings/session");
  await expect(page.getByText("Delete to Trash")).toBeVisible();
  await expect(page.getByText("Confirm Before Delete")).toBeVisible();
  await expect(page.getByText("Trash Retention (days)")).toBeVisible();
  await expect(page.getByText("Idle auto-stop")).toHaveCount(0);
  await expect(page.getByText("Default profile")).toHaveCount(0);

  await page.goto("/settings/theme");
  await expect(page.locator("button:visible", { hasText: "Theme" }).first()).toBeVisible();
  await expect(page.getByText("Color mode")).toHaveCount(0);
  await expect(page.getByText("Idle decay")).toHaveCount(0);

  // MCP and Plugins render read-only.
  await page.goto("/settings/mcp");
  await expect(page.getByRole("heading", { name: "MCP Servers" }).first()).toBeVisible();
  await expect(page.getByRole("button", { name: /resolve dup/ })).toHaveCount(0);
  await expect(page.getByRole("button", { name: /keep old/ })).toHaveCount(0);
  await expect(page.getByRole("button", { name: /drop old/ })).toHaveCount(0);

  await page.goto("/settings/plugins");
  await expect(page.getByText("Acme").first()).toBeVisible();
  await expect(page.getByTestId("plugins-tab-marketplace")).toHaveCount(0);
  await expect(page.getByTestId("plugins-check-updates")).toHaveCount(0);
  await expect(page.getByRole("switch", { name: /Enable Acme/ })).toHaveCount(0);
  await expect(page.getByTestId("plugin-uninstall-acme")).toHaveCount(0);
});

test("new-session wizard is name-only in CityHall mode", async ({ page }) => {
  await installCityHallMocks(page);
  await page.setViewportSize({ width: 1280, height: 900 });
  await page.goto("/");

  await expect(page.getByText("New session").first()).toBeVisible();
  await expect(page.getByText("Clone URL")).toHaveCount(0);

  await openWizard(page);

  await expect(wizard(page).getByPlaceholder("Auto-generated if empty")).toBeVisible();
  await expect(wizard(page).getByText("Which AI agent?")).toHaveCount(0);
  await expect(wizard(page).getByRole("button", { name: "More options" })).toHaveCount(0);

  await expect(wizard(page).getByRole("button", { name: /Launch session/ })).toBeEnabled();
});
