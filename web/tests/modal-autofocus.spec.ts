import type { Page } from "@playwright/test";
import { mockSettingsApis } from "./helpers/apiMocks";
import { test, expect } from "./helpers/mockedTest";

async function openMcpSettings(page: Page) {
  await mockSettingsApis(page);
  await page.route(
    (url) => url.pathname === "/api/mcp/servers",
    (r) =>
      r.fulfill({
        json: {
          agent: "claude",
          effective: [{ name: "github", transport: "stdio", command: "gh-mcp", provenance: "global" }],
          keptOnRemoval: [],
          conflicts: [],
          driftPaused: false,
        },
      }),
  );
  await page.goto("/settings/mcp");
  await expect(page.getByRole("button", { name: "Add server" })).toBeEnabled();
}

test.describe("modals focus their first editable field", () => {
  test("MCP form focuses Name when adding and Transport when editing", async ({ page }) => {
    await openMcpSettings(page);
    await page.getByRole("button", { name: "Add server" }).click();
    await expect(page.getByLabel("Name", { exact: true })).toBeFocused();
    await page.getByRole("button", { name: "Cancel" }).click();
    await expect(page.getByLabel("Name", { exact: true })).toHaveCount(0);
    await page.getByRole("button", { name: "edit github" }).click();
    await expect(page.getByLabel("Transport")).toBeFocused();
  });

  test("adding a project focuses Path", async ({ page }) => {
    await page.route("**/api/about", (r) => r.fulfill({ json: { profile: "default" } }));
    await page.route(/\/api\/sessions(\?.*)?$/, (r) =>
      r.fulfill({
        headers: { "aoe-runtime-epoch": "mock-runtime", "aoe-runtime-revision": "1" },
        json: { sessions: [], workspace_ordering: [] },
      }),
    );
    await page.route("**/api/profiles", (r) => r.fulfill({ json: [{ name: "default", is_default: true }] }));
    await page.route("**/api/projects*", (r) =>
      r.fulfill({ json: [{ name: "saved-repo", path: "/work/saved-repo", scope: "global", pinned: false }] }),
    );
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/");
    await page.getByTestId("sidebar-projects-add").click();
    await expect(page.getByTestId("project-form-modal")).toBeVisible();
    await expect(page.getByPlaceholder("/path/to/repo")).toBeFocused();
  });
});

test.describe("modal autofocus on touch", () => {
  test.use({ hasTouch: true, isMobile: true });

  test("the MCP form focuses nothing, so the keyboard stays down", async ({ page }) => {
    await openMcpSettings(page);
    await page.getByRole("button", { name: "Add server" }).click();
    await expect(page.getByLabel("Name", { exact: true })).toBeVisible();
    await expect(page.getByLabel("Name", { exact: true })).not.toBeFocused();
  });
});
