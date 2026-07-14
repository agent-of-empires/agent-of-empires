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

const THEME_SCHEMA = [
  {
    section: "theme",
    field: "name",
    label: "Theme",
    category: "theme",
    description: "",
    profile_overridable: true,
    validation: { rule: "none" },
    widget: { kind: "select", options: [{ value: "dark", label: "dark" }] },
    advanced: false,
    web_write: { policy: "allow" },
  },
];

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
    (r) => r.fulfill({ json: THEME_SCHEMA }),
  );
  await page.route(
    (url) => url.pathname === "/api/settings",
    (r) => r.fulfill({ json: { theme: { name: "dark" } } }),
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

test("new-session wizard is name-only in CityHall mode", async ({ page }) => {
  await installCityHallMocks(page);
  await page.setViewportSize({ width: 1280, height: 900 });
  await page.goto("/");
  await openWizard(page);

  // The title input remains; project picker, agent picker, and the More
  // options fold are all hidden because the server derives them.
  await expect(wizard(page).getByPlaceholder("Auto-generated if empty")).toBeVisible();
  await expect(wizard(page).getByText("Which AI agent?")).toHaveCount(0);
  await expect(wizard(page).getByRole("button", { name: "More options" })).toHaveCount(0);

  // Launch is enabled without selecting a project (server derives the set).
  await expect(wizard(page).getByRole("button", { name: /Launch session/ })).toBeEnabled();
});
