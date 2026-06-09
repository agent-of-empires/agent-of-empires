import { test, expect } from "./helpers/mockedTest";
import { Page } from "@playwright/test";

// Wizard Project step (#1219). Covers the three-tab layout (recent /
// browse / clone), DirectoryBrowser integration on the browse tab, and
// the Clone-from-URL form's enable-by-URL gating. The attach-existing
// and base-branch flows are covered separately in wizard-attach-existing
// and wizard-base-branch specs.

async function mockBaseApis(page: Page) {
  await page.route("**/api/login/status", (r) => r.fulfill({ json: { required: false, authenticated: true } }));
  for (const path of ["settings", "themes", "profiles", "groups", "devices", "about", "system/update-status"]) {
    await page.route(`**/api/${path}`, (r) =>
      r.fulfill({
        json: path === "settings" || path === "about" || path === "system/update-status" ? {} : [],
      }),
    );
  }
  await page.route("**/api/docker/status", (r) => r.fulfill({ json: { available: false, runtime: null } }));
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
  await page.route("**/api/filesystem/home", (r) => r.fulfill({ json: { path: "/home/user" } }));
}

function seedRecentSession() {
  return {
    sessions: [
      {
        id: "seed",
        title: "seed",
        project_path: "/tmp/example",
        group_path: "/tmp",
        tool: "claude",
        status: "Idle",
        yolo_mode: false,
        created_at: new Date().toISOString(),
        last_accessed_at: new Date().toISOString(),
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
  };
}

async function openWizard(page: Page) {
  await page.locator("body").click();
  await page.keyboard.press("n");
  await expect(page.getByRole("heading", { name: "New session" })).toBeVisible();
}

test.describe("Wizard project step (#1219)", () => {
  test("Recent tab is the default when sessions exist", async ({ page }) => {
    await mockBaseApis(page);
    await page.route("**/api/sessions", (r) => r.fulfill({ json: seedRecentSession() }));
    await page.setViewportSize({ width: 1280, height: 900 });
    await page.goto("/");
    await openWizard(page);
    await expect(page.getByRole("button", { name: "Recent", exact: true })).toBeVisible();
    await expect(page.getByRole("button", { name: "Browse", exact: true })).toBeVisible();
    await expect(page.getByRole("button", { name: "Clone URL", exact: true })).toBeVisible();
    // Recent entry visible (the seeded /tmp/example session).
    const recent = page.getByRole("button").filter({ hasText: "/tmp/example" }).first();
    await expect(recent).toBeVisible();
  });

  test("Browse tab defaults when no recents", async ({ page }) => {
    await mockBaseApis(page);
    await page.route("**/api/sessions", (r) => r.fulfill({ json: { sessions: [], workspace_ordering: [] } }));
    await page.route("**/api/filesystem/browse**", (r) => r.fulfill({ json: { entries: [], has_more: false } }));
    await page.setViewportSize({ width: 1280, height: 900 });
    await page.goto("/");
    await openWizard(page);
    // No Recent tab when there are zero recents.
    await expect(page.getByRole("button", { name: "Recent" })).toHaveCount(0);
    // Browse tab is active: directory-browser breadcrumb root (`~`) renders.
    await expect(page.getByTitle("Go to home")).toBeVisible();
  });

  test("switching to Browse tab renders DirectoryBrowser and selecting a repo populates path", async ({ page }) => {
    await mockBaseApis(page);
    await page.route("**/api/sessions", (r) => r.fulfill({ json: seedRecentSession() }));
    await page.route("**/api/filesystem/browse**", (r) =>
      r.fulfill({
        json: {
          entries: [
            {
              name: "my-repo",
              path: "/home/user/my-repo",
              is_dir: true,
              is_git_repo: true,
            },
            {
              name: "docs",
              path: "/home/user/docs",
              is_dir: true,
              is_git_repo: false,
            },
          ],
          has_more: false,
        },
      }),
    );
    await page.setViewportSize({ width: 1280, height: 900 });
    await page.goto("/");
    await openWizard(page);
    await page.getByRole("button", { name: "Browse", exact: true }).click();
    // Wait for filesystem listing to render the repo entry.
    const repoEntry = page.getByRole("option").filter({ hasText: "my-repo" });
    await expect(repoEntry).toBeVisible();
    await repoEntry.click();
    // Picking the repo flips back to the Recent tab and shows the selected path.
    await expect(page.getByText("Selected project")).toBeVisible();
    await expect(page.getByText("/home/user/my-repo", { exact: false })).toBeVisible();
  });

  test("Browse tab can load entries beyond the first page", async ({ page }) => {
    await mockBaseApis(page);
    await page.route("**/api/sessions", (r) => r.fulfill({ json: { sessions: [], workspace_ordering: [] } }));
    const entries = Array.from({ length: 125 }, (_, i) => {
      const n = String(i + 1).padStart(3, "0");
      return {
        name: `project-${n}`,
        path: `/home/user/project-${n}`,
        is_dir: true,
        is_git_repo: i === 124,
      };
    });
    await page.route("**/api/filesystem/browse**", (r) => {
      const limit = Number(new URL(r.request().url()).searchParams.get("limit") ?? "100");
      r.fulfill({
        json: {
          entries: entries.slice(0, limit),
          has_more: entries.length > limit,
        },
      });
    });

    await page.setViewportSize({ width: 1280, height: 900 });
    await page.goto("/");
    await openWizard(page);

    await expect(page.getByRole("option").filter({ hasText: "project-001" })).toBeVisible();
    await expect(page.getByRole("option").filter({ hasText: "project-125" })).toHaveCount(0);
    await expect(page.getByText("Showing first 100 entries.")).toBeVisible();

    await page.getByRole("button", { name: "Load 100 more" }).click();

    const finalEntry = page.getByRole("option").filter({ hasText: "project-125" });
    await expect(finalEntry).toBeVisible();
    await expect(page.getByText("Load 100 more")).toHaveCount(0);
    await finalEntry.click();
    await expect(page.getByText("Selected project")).toBeVisible();
    await expect(page.getByText("/home/user/project-125", { exact: false })).toBeVisible();
  });

  test("Browse tab filters entries beyond the first page on the server", async ({ page }) => {
    await mockBaseApis(page);
    await page.route("**/api/sessions", (r) => r.fulfill({ json: { sessions: [], workspace_ordering: [] } }));
    const entries = [
      ...Array.from({ length: 124 }, (_, i) => {
        const n = String(i + 1).padStart(3, "0");
        return {
          name: `project-${n}`,
          path: `/home/user/project-${n}`,
          is_dir: true,
          is_git_repo: false,
        };
      }),
      {
        name: "z-project",
        path: "/home/user/z-project",
        is_dir: true,
        is_git_repo: true,
      },
    ];
    await page.route("**/api/filesystem/browse**", (r) => {
      const params = new URL(r.request().url()).searchParams;
      const limit = Number(params.get("limit") ?? "100");
      const filter = params.get("filter")?.toLowerCase() ?? "";
      const filtered = filter ? entries.filter((e) => e.name.toLowerCase().includes(filter)) : entries;
      r.fulfill({
        json: {
          entries: filtered.slice(0, limit),
          has_more: filtered.length > limit,
        },
      });
    });

    await page.setViewportSize({ width: 1280, height: 900 });
    await page.goto("/");
    await openWizard(page);

    await expect(page.getByRole("option").filter({ hasText: "project-001" })).toBeVisible();
    await expect(page.getByRole("option").filter({ hasText: "z-project" })).toHaveCount(0);

    await page.getByPlaceholder("Type to filter...").fill("z");

    const zProject = page.getByRole("option").filter({ hasText: "z-project" });
    await expect(zProject).toBeVisible();
    await expect(page.getByRole("button", { name: "Load 100 more" })).toHaveCount(0);
    await zProject.click();
    await expect(page.getByText("Selected project")).toBeVisible();
    await expect(page.getByText("/home/user/z-project", { exact: false })).toBeVisible();
  });

  test("Clone tab: clone button disabled until URL non-empty", async ({ page }) => {
    await mockBaseApis(page);
    await page.route("**/api/sessions", (r) => r.fulfill({ json: seedRecentSession() }));
    await page.setViewportSize({ width: 1280, height: 900 });
    await page.goto("/");
    await openWizard(page);
    await page.getByRole("button", { name: "Clone URL", exact: true }).click();
    const cloneBtn = page.getByRole("button", { name: "Clone repository" });
    await expect(cloneBtn).toBeDisabled();
    const urlInput = page.locator("#clone-url");
    await urlInput.fill("https://github.com/user/repo.git");
    await expect(cloneBtn).toBeEnabled();
    await urlInput.fill("   ");
    await expect(cloneBtn).toBeDisabled();
  });

  test("Clone tab: Advanced reveals destination + shallow toggle", async ({ page }) => {
    await mockBaseApis(page);
    await page.route("**/api/sessions", (r) => r.fulfill({ json: seedRecentSession() }));
    await page.setViewportSize({ width: 1280, height: 900 });
    await page.goto("/");
    await openWizard(page);
    await page.getByRole("button", { name: "Clone URL", exact: true }).click();
    // Destination field is hidden until Advanced is expanded.
    await expect(page.locator("#clone-dest")).toHaveCount(0);
    await page.getByRole("button", { name: /Advanced/ }).click();
    await expect(page.locator("#clone-dest")).toBeVisible();
    await expect(page.locator("label", { hasText: "Shallow clone" }).locator("input[type=checkbox]")).toBeVisible();
  });
});
