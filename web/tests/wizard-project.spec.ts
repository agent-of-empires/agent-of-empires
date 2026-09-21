// Wizard project picker (#1219): Recent / Browse / Clone tabs, extra repos, and scratch sessions (#1324).

import type { Page } from "@playwright/test";
import { test, expect } from "./helpers/mockedTest";
import { expandMoreOptions, mockWizardApis, openWizard, sessionStub, startWizard, wizard } from "./helpers/wizard";

const option = (page: Page, text: string) => page.getByRole("option").filter({ hasText: text });

/** Mock a paginated home listing that honors `limit` and `filter` like the server. */
async function mockBrowse(page: Page, entries: { name: string; is_git_repo?: boolean }[]) {
  await page.route("**/api/filesystem/home", (r) => r.fulfill({ json: { path: "/home/user" } }));
  await page.route("**/api/filesystem/browse**", (r) => {
    const params = new URL(r.request().url()).searchParams;
    const limit = Number(params.get("limit") ?? "100");
    const filter = params.get("filter")?.toLowerCase() ?? "";
    const matching = entries
      .filter((e) => e.name.toLowerCase().includes(filter))
      .map((e) => ({ path: `/home/user/${e.name}`, is_dir: true, is_git_repo: false, ...e }));
    return r.fulfill({ json: { entries: matching.slice(0, limit), has_more: matching.length > limit } });
  });
}

async function expectSelected(page: Page, path: string) {
  await expect(page.getByText("Selected project")).toBeVisible();
  await expect(page.getByText(path, { exact: false })).toBeVisible();
}

test.describe("project tabs", () => {
  test("Recent tab is the default when sessions exist", async ({ page }) => {
    await startWizard(page, { project: false });
    await openWizard(page);
    for (const tab of ["Recent", "Browse", "Clone URL"]) {
      await expect(page.getByRole("button", { name: tab, exact: true })).toBeVisible();
    }
    await expect(page.getByRole("button").filter({ hasText: "/tmp/example" }).first()).toBeVisible();
  });

  test("Browse tab defaults when no recents", async ({ page }) => {
    await mockBrowse(page, []);
    await startWizard(page, { sessions: [], project: false });
    await openWizard(page);
    await expect(page.getByRole("button", { name: "Recent" })).toHaveCount(0);
    await expect(page.getByTitle("Go to home")).toBeVisible();
  });

  test("saved projects show under the Recent tab even with no sessions (#2140)", async ({ page }) => {
    await startWizard(page, {
      sessions: [],
      projects: [{ name: "my-saved-repo", path: "/srv/my-saved-repo", scope: "global" }],
      project: false,
    });
    await openWizard(page);
    await expect(page.getByRole("button", { name: "Recent", exact: true })).toBeVisible();
    await expect(page.getByText("Saved projects")).toBeVisible();
    // Scoped: the sidebar Projects section (#2212) shows the same project.
    const savedRow = wizard(page).getByRole("button").filter({ hasText: "/srv/my-saved-repo" });
    await savedRow.click();
    await expect(savedRow).toHaveClass(/border-brand-600/);
    await expect(page.getByText("Selected project")).toHaveCount(0);
    await expect(page.getByRole("button", { name: /Launch session/ })).toBeEnabled();
  });

  test("switching to Browse tab renders DirectoryBrowser and selecting a repo populates path", async ({ page }) => {
    await mockBrowse(page, [{ name: "my-repo", is_git_repo: true }, { name: "docs" }]);
    await startWizard(page, { project: false });
    await openWizard(page);
    await page.getByRole("button", { name: "Browse", exact: true }).click();
    await option(page, "my-repo").click();
    await expectSelected(page, "/home/user/my-repo");
  });

  test("Browse tab pages beyond the first 100 entries and filters server-side", async ({ page }) => {
    const numbered = Array.from({ length: 124 }, (_, i) => ({ name: `project-${String(i + 1).padStart(3, "0")}` }));
    await mockBrowse(page, [
      ...numbered,
      { name: "project-125", is_git_repo: true },
      { name: "z-project", is_git_repo: true },
    ]);
    await startWizard(page, { sessions: [], project: false });

    await openWizard(page);
    await expect(option(page, "project-001")).toBeVisible();
    await expect(option(page, "project-125")).toHaveCount(0);
    await expect(page.getByText("Showing first 100 entries.")).toBeVisible();
    await page.getByRole("button", { name: "Load 100 more" }).click();
    await expect(option(page, "project-125")).toBeVisible();
    await expect(page.getByText("Load 100 more")).toHaveCount(0);
    await option(page, "project-125").click();
    await expectSelected(page, "/home/user/project-125");

    await page.getByRole("button", { name: "Close" }).click();
    await openWizard(page);
    await expect(option(page, "project-001")).toBeVisible();
    await expect(option(page, "z-project")).toHaveCount(0);
    await page.getByPlaceholder("Type to filter...").fill("z");
    await expect(option(page, "z-project")).toBeVisible();
    await expect(page.getByRole("button", { name: "Load 100 more" })).toHaveCount(0);
    await option(page, "z-project").click();
    await expectSelected(page, "/home/user/z-project");
  });

  test("Clone tab gates the button on a non-blank URL and Advanced reveals destination + shallow", async ({ page }) => {
    await startWizard(page, { project: false });
    await openWizard(page);
    await page.getByRole("button", { name: "Clone URL", exact: true }).click();
    const cloneBtn = page.getByRole("button", { name: "Clone repository" });
    const urlInput = page.locator("#clone-url");
    await expect(cloneBtn).toBeDisabled();
    await urlInput.fill("https://github.com/user/repo.git");
    await expect(cloneBtn).toBeEnabled();
    await urlInput.fill("   ");
    await expect(cloneBtn).toBeDisabled();

    await expect(page.locator("#clone-dest")).toHaveCount(0);
    await page.getByRole("button", { name: /Advanced/ }).click();
    await expect(page.locator("#clone-dest")).toBeVisible();
    await expect(page.locator("label", { hasText: "Shallow clone" }).locator("input[type=checkbox]")).toBeVisible();
  });
});

test.describe("extra repos", () => {
  async function openPicker(page: Page) {
    await startWizard(page, {
      projects: [
        { name: "primary", path: "/tmp/example", scope: "global" },
        { name: "shared-lib", path: "/tmp/shared-lib", scope: "global" },
        { name: "docs", path: "/tmp/docs", scope: "profile" },
      ],
    });
    await expect(page.getByText("Extra repos (optional)")).toBeVisible();
    // Scoped: the sidebar and the step's Saved projects list render the same projects.
    const picker = page.getByTestId("extra-repos-picker");
    const chip = (name: string) => picker.getByRole("button").filter({ hasText: new RegExp(`^${name}`) });
    return { picker, chip, input: page.getByPlaceholder("/path/to/another/repo") };
  }

  test("saved projects render without the primary and toggle as chips", async ({ page }) => {
    const { picker, chip } = await openPicker(page);
    await expect(picker.getByText("Saved projects")).toBeVisible();
    await expect(chip("primary")).toHaveCount(0);
    await expect(chip("docs")).toBeVisible();
    await chip("shared-lib").first().click();
    await expect(page.getByText("1 selected")).toBeVisible();
    await page.getByRole("button", { name: "Remove shared-lib" }).click();
    await expect(page.getByText("none")).toBeVisible();
  });

  test("the picker's own search box filters its saved-projects list", async ({ page }) => {
    // #3743. Scoped: the Recent tab's search box has the same accessible name.
    const { picker, chip } = await openPicker(page);
    await picker.getByLabel("Search projects").fill("shared");
    await expect(chip("shared-lib")).toBeVisible();
    await expect(chip("docs")).toHaveCount(0);
  });

  test("free-text paths add via Enter or the Add button, which needs input", async ({ page }) => {
    const { input } = await openPicker(page);
    await input.fill("/tmp/manual-path");
    await input.press("Enter");
    await expect(page.getByText("1 selected")).toBeVisible();
    await expect(input).toHaveValue("");
    await page.getByRole("button", { name: "Remove manual-path" }).click();
    await expect(page.getByText("none")).toBeVisible();

    const addBtn = page.getByRole("button", { name: "Add", exact: true });
    await expect(addBtn).toBeDisabled();
    await input.fill("/tmp/x");
    await expect(addBtn).toBeEnabled();
    await addBtn.click();
    await expect(page.getByText("1 selected")).toBeVisible();
  });

  test("attempting to add the primary path as a free-text entry is a no-op", async ({ page }) => {
    const { input } = await openPicker(page);
    await input.fill("/tmp/example");
    await page.getByRole("button", { name: "Add", exact: true }).click();
    await expect(page.getByText("none")).toBeVisible();
  });
});

test.describe("scratch sessions", () => {
  test("Skip project folder defaults off, enables Launch without a path, and hides path sources and worktrees", async ({
    page,
  }) => {
    await startWizard(page, { project: false });
    await openWizard(page);
    const w = wizard(page);
    const toggle = w.getByRole("switch", { name: "Skip project folder" });
    const launchButton = w.getByRole("button", { name: /Launch session/ });
    await expect(toggle).toHaveAttribute("aria-checked", "false");
    await expect(launchButton).toBeDisabled();

    await toggle.click();
    await expect(launchButton).toBeEnabled();
    await expect(w.getByText(/Scratch session/).first()).toBeVisible();
    await expect(w.getByRole("button", { name: "Browse" })).toBeHidden();
    // A scratch directory is not a git repo.
    await expandMoreOptions(page);
    await expect(w.getByText(/Scratch sessions do not use git worktrees/)).toBeVisible();
    await expect(w.getByRole("switch", { name: /Create a worktree/i })).toHaveCount(0);
  });

  test("Cmd+Shift+N opens the wizard with scratch on; Cmd+Enter launches", async ({ page }) => {
    const created = await startWizard(page, { project: false });
    // The New session button doubles as a signal that the global shortcut handler is registered.
    await expect(page.getByRole("button", { name: /New session/ }).first()).toBeVisible();
    await page.keyboard.press("ControlOrMeta+Shift+KeyN");
    const w = wizard(page);
    await expect(w.getByRole("button", { name: /Launch session/ })).toBeVisible();
    await expect(w.getByText(/Scratch session/).first()).toBeVisible();
    await expect(w.getByRole("switch", { name: "Skip project folder" })).toHaveAttribute("aria-checked", "true");
    // Launch and its shortcut stay disabled until the profile defaults settle.
    await expect(w.getByRole("button", { name: /Launch session/ })).toBeEnabled();
    await page.keyboard.press("ControlOrMeta+Enter");
    // The server provisions the directory, so no path is sent.
    await expect.poll(() => created[0]?.scratch).toBe(true);
    expect(created[0]?.path).toBe("");
    expect(created[0]?.tool).toBe("claude");
  });

  test("scratch sessions render in a single synthetic Scratch group", async ({ page }) => {
    const scratch = (id: string, title: string) =>
      sessionStub({
        id,
        title,
        project_path: `/home/user/.config/agent-of-empires/scratch/${id}`,
        group_path: null,
        scratch: true,
      });
    await mockWizardApis(page, {
      sessions: [
        sessionStub({
          id: "alpha",
          title: "alpha-session",
          project_path: "/home/user/repo-alpha",
          group_path: "/home/user",
        }),
        scratch("scr-1", "scratch-one"),
        scratch("scr-2", "scratch-two"),
      ],
    });
    await page.setViewportSize({ width: 1280, height: 900 });
    await page.goto("/");
    // Each scratch session has its own directory; without the synthetic group each would get a header.
    await expect(page.locator("[data-testid='sidebar-group-header']")).toHaveCount(2);
    await expect(page.getByText("repo-alpha")).toBeVisible();
    await expect(page.locator("[data-testid='sidebar-group-header'][data-group-id='__scratch__']")).toContainText(
      "Scratch",
    );
    await expect(page.locator("[data-testid='sidebar-session-row']")).toHaveCount(3);
    for (const title of ["alpha-session", "scratch-one", "scratch-two"])
      await expect(page.getByText(title)).toBeVisible();
  });
});
