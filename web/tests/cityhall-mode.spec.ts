// #7: CityHall client mode (AOE_CITYHALL_MODE) locks the dashboard down to a
// composer + structured-view end-user client. The frontend reacts to
// serverAbout.cityhall_mode via getClientCapabilities; this spec pins the two
// most visible gates through real URL routing: Settings collapses to the Theme
// tab, and the new-session wizard collapses to a name-only form (project /
// agent / more-options controls gone, Launch enabled without picking a
// project). The server derives the project set, view, and agent, so the client
// only asks for a title. Pane hiding (diff/terminal) and the server-side
// endpoint lockdown are covered by unit tests and the manual test steps.

import { test, expect } from "./helpers/mockedTest";
import type { Page } from "@playwright/test";
import { openWizard, wizard } from "./helpers/wizard";

const SCHEMA = [
  {
    section: "theme",
    field: "name",
    label: "Theme",
    widget: { kind: "select", options: [{ value: "dark", label: "dark" }] },
  },
  // color_mode / idle_decay must be hidden in CityHall mode.
  { section: "theme", field: "color_mode", label: "Color mode", widget: { kind: "select", options: [] } },
  { section: "theme", field: "idle_decay_minutes", label: "Idle decay", widget: { kind: "number" } },
  // Session tab is curated to the trash cluster only.
  { section: "session", field: "delete_to_trash", label: "Delete to Trash", widget: { kind: "toggle" } },
  { section: "session", field: "confirm_delete", label: "Confirm Before Delete", widget: { kind: "toggle" } },
  {
    section: "session",
    field: "trash_retention_days",
    label: "Trash Retention (days)",
    widget: { kind: "number" },
  },
  // A non-trash session field that must NOT appear under the curated tab.
  { section: "session", field: "idle_auto_stop", label: "Idle auto-stop", widget: { kind: "toggle" } },
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
  await page.route(
    (url) => url.pathname === "/api/about",
    (r) =>
      r.fulfill({
        json: { read_only: false, auth_mode: "none", behind_tunnel: false, profile: "main", cityhall_mode: true },
      }),
  );
  await page.route(
    (url) => url.pathname === "/api/sessions",
    (r) => r.fulfill({ json: { sessions: [], workspace_ordering: [] } }),
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
    (r) =>
      r.fulfill({
        json: { theme: { name: "dark" }, session: { delete_to_trash: true, trash_retention_days: 30 } },
      }),
  );
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
  // MCP + plugins responses shaped so their mutating controls WOULD render in
  // normal mode (a conflict, a kept-on-removal server, a removable plugin); the
  // CityHall read-only path must suppress them.
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

test("Settings is curated to the CityHall subset", async ({ page }) => {
  await installCityHallMocks(page);
  await page.goto("/settings");

  // The curated tabs are present; advanced/config tabs and the profile
  // switcher are gone. Scope tab assertions to the visible strip (desktop +
  // mobile both render one).
  for (const label of ["Theme", "Sessions", "MCP servers", "Telemetry", "Plugins"]) {
    await expect(page.locator("button:visible", { hasText: label }).first()).toBeVisible();
  }
  await expect(page.getByText("Sandbox")).toHaveCount(0);
  await expect(page.getByText("Worktree")).toHaveCount(0);
  await expect(page.getByText("Security")).toHaveCount(0);
  // Profiles tab and the header profile switcher are both absent.
  await expect(page.getByText("Profiles")).toHaveCount(0);
});

test("CityHall Sessions tab shows only the trash options", async ({ page }) => {
  await installCityHallMocks(page);
  await page.goto("/settings/session");

  // The trash cluster is present.
  await expect(page.getByText("Delete to Trash")).toBeVisible();
  await expect(page.getByText("Confirm Before Delete")).toBeVisible();
  await expect(page.getByText("Trash Retention (days)")).toBeVisible();
  // Non-trash session settings and the default-profile selector are not.
  await expect(page.getByText("Idle auto-stop")).toHaveCount(0);
  await expect(page.getByText("Default profile")).toHaveCount(0);
});

test("CityHall Theme tab hides color-mode and idle-decay", async ({ page }) => {
  await installCityHallMocks(page);
  await page.goto("/settings/theme");

  await expect(page.locator("button:visible", { hasText: "Theme" }).first()).toBeVisible();
  await expect(page.getByText("Color mode")).toHaveCount(0);
  await expect(page.getByText("Idle decay")).toHaveCount(0);
});

test("CityHall MCP + Plugins tabs render read-only", async ({ page }) => {
  await installCityHallMocks(page);

  // MCP: the informational rows show, but Resolve / Keep / Drop are gone.
  await page.goto("/settings/mcp");
  await expect(page.getByRole("heading", { name: "MCP Servers" }).first()).toBeVisible();
  await expect(page.getByRole("button", { name: /resolve dup/ })).toHaveCount(0);
  await expect(page.getByRole("button", { name: /keep old/ })).toHaveCount(0);
  await expect(page.getByRole("button", { name: /drop old/ })).toHaveCount(0);

  // Plugins: the installed plugin shows, but the marketplace tab, the enable
  // toggle, and the uninstall control are all suppressed.
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

  // The homescreen "Clone URL" action is a project-management affordance and
  // is hidden; "New session" remains.
  await expect(page.getByText("New session").first()).toBeVisible();
  await expect(page.getByText("Clone URL")).toHaveCount(0);

  await openWizard(page);

  // The title input remains; project picker, agent picker, and the More
  // options fold are all hidden because the server derives them.
  await expect(wizard(page).getByPlaceholder("Auto-generated if empty")).toBeVisible();
  await expect(wizard(page).getByText("Which AI agent?")).toHaveCount(0);
  await expect(wizard(page).getByRole("button", { name: "More options" })).toHaveCount(0);

  // Launch is enabled without selecting a project (server derives the set).
  await expect(wizard(page).getByRole("button", { name: /Launch session/ })).toBeEnabled();
});
