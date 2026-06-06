import { test, expect } from "./helpers/mockedTest";
import { Page } from "@playwright/test";

// Wizard Extra repos picker (#1219). Lives under the Project step once
// a primary path is selected. Covers chip toggle on registered
// projects, free-text path add via input + Enter / Add button, removal
// via the X button on selected chips, and the primary-path filtering
// rule that hides the selected project from the registered list.

interface MockOptions {
  projects?: Array<{ name: string; path: string; scope: "global" | "profile" }>;
}

async function mockApis(page: Page, opts: MockOptions = {}) {
  await page.route("**/api/login/status", (r) =>
    r.fulfill({ json: { required: false, authenticated: true } }),
  );
  for (const path of [
    "settings",
    "themes",
    "profiles",
    "groups",
    "devices",
    "about",
    "system/update-status",
  ]) {
    await page.route(`**/api/${path}`, (r) =>
      r.fulfill({
        json:
          path === "settings" ||
          path === "about" ||
          path === "system/update-status"
            ? {}
            : [],
      }),
    );
  }
  await page.route("**/api/docker/status", (r) =>
    r.fulfill({ json: { available: false, runtime: null } }),
  );
  await page.route("**/api/agents", (r) =>
    r.fulfill({
      json: [
        {
          name: "claude",
          binary: "claude",
          host_only: false,
          installed: true,
          install_hint: "",
        },
      ],
    }),
  );
  await page.route("**/api/projects**", (r) =>
    r.fulfill({
      json: opts.projects ?? [
        { name: "primary", path: "/tmp/example", scope: "global" },
        { name: "shared-lib", path: "/tmp/shared-lib", scope: "global" },
        { name: "docs", path: "/tmp/docs", scope: "profile" },
      ],
    }),
  );
  await page.route("**/api/sessions", (r) =>
    r.fulfill({
      json: {
        sessions: [
          {
            id: "seed-session",
            title: "seed",
            project_path: "/tmp/example",
            group_path: "/tmp",
            tool: "claude",
            status: "Idle",
            yolo_mode: false,
            created_at: new Date().toISOString(),
            last_accessed_at: null,
            last_error: null,
            branch: null,
            main_repo_path: null,
            is_sandboxed: false,
            has_terminal: true,
            profile: "default",
            workspace_repos: [],
          },
        ],
        workspace_ordering: [],
      },
    }),
  );
}

async function openProjectStepWithPath(page: Page) {
  await page.locator("body").click();
  await page.keyboard.press("n");
  await expect(
    page.getByRole("heading", { name: "New session" }),
  ).toBeVisible();
  const recent = page
    .getByRole("button")
    .filter({ hasText: "/tmp/example" })
    .first();
  await recent.waitFor({ state: "visible", timeout: 5000 });
  await recent.click();
  // Extra repos picker section becomes visible once a primary path is set.
  await expect(page.getByText("Extra repos (optional)")).toBeVisible();
}

test.describe("Wizard extra repos picker (#1219)", () => {
  test("registered projects render and the primary path is filtered out", async ({
    page,
  }) => {
    await mockApis(page);
    await page.setViewportSize({ width: 1280, height: 900 });
    await page.goto("/");
    await openProjectStepWithPath(page);
    await expect(page.getByText("Registered projects")).toBeVisible();
    // The primary entry (matching /tmp/example) is hidden from the picker
    // so users can't accidentally duplicate it.
    await expect(
      page.getByRole("button").filter({ hasText: /^primary$/ }),
    ).toHaveCount(0);
    await expect(
      page.getByRole("button").filter({ hasText: /^shared-lib/ }),
    ).toBeVisible();
    await expect(
      page.getByRole("button").filter({ hasText: /^docs/ }),
    ).toBeVisible();
  });

  test("clicking a registered project chip toggles selection", async ({
    page,
  }) => {
    await mockApis(page);
    await page.setViewportSize({ width: 1280, height: 900 });
    await page.goto("/");
    await openProjectStepWithPath(page);
    const sharedLib = page
      .getByRole("button")
      .filter({ hasText: /^shared-lib/ })
      .first();
    await sharedLib.click();
    await expect(page.getByText("1 selected")).toBeVisible();
    // Removing via the dedicated remove button on the selected chip.
    await page.getByRole("button", { name: "Remove shared-lib" }).click();
    await expect(page.getByText("none")).toBeVisible();
  });

  test("free-text path: typing + Enter appends a chip and clears the input", async ({
    page,
  }) => {
    await mockApis(page);
    await page.setViewportSize({ width: 1280, height: 900 });
    await page.goto("/");
    await openProjectStepWithPath(page);
    const input = page.getByPlaceholder("/path/to/another/repo");
    await input.fill("/tmp/manual-path");
    await input.press("Enter");
    await expect(page.getByText("1 selected")).toBeVisible();
    await expect(input).toHaveValue("");
    // Chip remove via × button (aria-label uses the trailing path segment).
    await page.getByRole("button", { name: "Remove manual-path" }).click();
    await expect(page.getByText("none")).toBeVisible();
  });

  test("free-text path: Add button stays disabled while input is empty", async ({
    page,
  }) => {
    await mockApis(page);
    await page.setViewportSize({ width: 1280, height: 900 });
    await page.goto("/");
    await openProjectStepWithPath(page);
    const addBtn = page.getByRole("button", { name: "Add", exact: true });
    await expect(addBtn).toBeDisabled();
    await page.getByPlaceholder("/path/to/another/repo").fill("/tmp/x");
    await expect(addBtn).toBeEnabled();
    await addBtn.click();
    await expect(page.getByText("1 selected")).toBeVisible();
  });

  test("attempting to add the primary path as a free-text entry is a no-op", async ({
    page,
  }) => {
    await mockApis(page);
    await page.setViewportSize({ width: 1280, height: 900 });
    await page.goto("/");
    await openProjectStepWithPath(page);
    await page.getByPlaceholder("/path/to/another/repo").fill("/tmp/example");
    await page.getByRole("button", { name: "Add", exact: true }).click();
    // Should not add a duplicate of the primary; counter stays at "none".
    await expect(page.getByText("none")).toBeVisible();
  });
});
