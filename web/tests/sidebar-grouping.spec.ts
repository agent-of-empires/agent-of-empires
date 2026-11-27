// Sidebar groupings (#1220, #1234, #1720, #5896): Repository, with the user's Groups nested under each repository
// header, and Groups, picked in the shared view menu, with per-grouping collapse persisted in localStorage.
// Bucketing is unit-tested in src/lib/__tests__/sidebarGroups.test.ts.

import { test, expect } from "./helpers/mockedTest";
import type { Locator, Page, Route } from "@playwright/test";
import { installSidebarMocks, type MockSessionInput } from "./helpers/sidebarMocks";
import { sessionResponse } from "./helpers/sessions";
import { launch, mockWizardApis, sessionStub, wizard } from "./helpers/wizard";

const HEADER = "[data-testid='sidebar-group-header']";
const ROW = "[data-testid='sidebar-session-row']";
const VIEW_TOGGLE = "[data-testid='sidebar-view-toggle']";

function twoRepoSessions(): MockSessionInput[] {
  return [
    { id: "s-a", title: "alpha-session", project_path: "/tmp/repo-alpha", branch: "feat/a" },
    { id: "s-b", title: "beta-session", project_path: "/tmp/repo-beta", branch: "feat/b" },
  ];
}

function groupedSessions(): MockSessionInput[] {
  return [
    { id: "s-f1", title: "feat-one", project_path: "/tmp/project", branch: "feat/one", group: "feature" },
    { id: "s-f2", title: "feat-two", project_path: "/tmp/project", branch: "feat/two", group: "feature" },
    { id: "s-r1", title: "refac-one", project_path: "/tmp/project", branch: "refac/one", group: "refactor" },
  ];
}

function nestedSessions(): MockSessionInput[] {
  return [
    { id: "s-f1", title: "feat-one", project_path: "/tmp/project", branch: "feat/one", group: "feature" },
    { id: "s-f2", title: "feat-two", project_path: "/tmp/project", branch: "feat/two", group: "feature" },
    { id: "s-x1", title: "fix-one", project_path: "/tmp/project", branch: "fix/one", group: "fix" },
    { id: "s-l1", title: "loose-one", project_path: "/tmp/project", branch: "loose/one" },
  ];
}

async function resolvedColor(page: Page, token: string): Promise<string> {
  return await page.evaluate((name) => {
    const probe = document.createElement("span");
    probe.style.color = `var(${name})`;
    document.body.append(probe);
    const rgb = getComputedStyle(probe).color;
    probe.remove();
    return rgb;
  }, token);
}

async function gotoDesktop(page: Page) {
  await page.setViewportSize({ width: 1280, height: 720 });
  await page.goto("/");
}

async function recordArchives(page: Page, ids: string[], sessions?: MockSessionInput[]) {
  await page.route("**/api/sessions/*/archive", async (route) => {
    const id = decodeURIComponent(new URL(route.request().url()).pathname.split("/").at(-2)!);
    const input = sessions?.find((session) => session.id === id);
    ids.push(id);
    await route.fulfill({
      json: input
        ? sessionResponse({
            id,
            title: input.title,
            project_path: input.project_path,
            branch: input.branch,
            group_path: input.group ?? "",
            ...input.fields,
          })
        : sessionResponse({ id }),
    });
  });
}

async function pickGrouping(page: Page, toggle: Locator, target: "project" | "group") {
  await toggle.click();
  await page.getByTestId(`sidebar-view-group-${target}`).click();
  await expect(toggle).toHaveAttribute("data-group-by", target);
}

test.describe("sidebar repo groups (#1220)", () => {
  test("two repos render as two groups; the filter narrows groups and rows by repo name", async ({ page }) => {
    await installSidebarMocks(page, { sessions: twoRepoSessions() });
    await gotoDesktop(page);

    await expect(page.locator(HEADER)).toHaveCount(2);
    await expect(page.getByText("repo-alpha")).toBeVisible();
    await expect(page.getByText("repo-beta")).toBeVisible();

    await expect(page.locator(ROW)).toHaveCount(2);
    await expect(page.locator('[data-tour="sidebar"]').getByText("alpha-session")).toBeVisible();
    await expect(page.locator('[data-tour="sidebar"]').getByText("beta-session")).toBeVisible();

    await page.getByTestId("sidebar-view-toggle").click();
    await page.getByTestId("sidebar-filter-toggle").click();
    const filter = page.locator("[data-testid='sidebar-filter-input']");
    await expect(filter).toBeVisible();

    await filter.fill("alpha");
    await expect(page.locator(HEADER)).toHaveCount(1);
    await expect(page.getByText("repo-alpha")).toBeVisible();
    await expect(page.getByText("repo-beta")).toBeHidden();

    // Clear the input rather than the toggle, which would also hide the input.
    await filter.fill("");
    await expect(page.locator(HEADER)).toHaveCount(2);

    await filter.fill("nonexistent-repo-xyz");
    await expect(page.getByText(/No matches for/)).toBeVisible();
    await expect(page.locator(ROW)).toHaveCount(0);
  });

  test("a collapsed group marks that it holds the open session (#3912)", async ({ page }) => {
    await installSidebarMocks(page, { sessions: twoRepoSessions() });
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/session/s-a");

    const alphaHeader = page.locator(HEADER, { has: page.getByText("repo-alpha") });
    const betaHeader = page.locator(HEADER, { has: page.getByText("repo-beta") });
    const expandBtn = alphaHeader.locator("button[aria-expanded]");

    // Expanded, the open session's row carries the frame instead of the header.
    await expect(expandBtn).toHaveAttribute("aria-expanded", "true");
    expect(await alphaHeader.getAttribute("class")).not.toContain("border-session-active");

    // Collapsed, the header uses the projected token; border-brand-600 fails contrast on catppuccin-latte.
    await expandBtn.click();
    await expect(page.locator('[data-tour="sidebar"]').getByText("alpha-session")).toBeHidden();
    expect(await alphaHeader.getAttribute("class")).toContain("border-session-active");
    expect(await betaHeader.getAttribute("class")).not.toContain("border-session-active");
    // toHaveCSS polls past the color transition.
    await expect(alphaHeader).toHaveCSS("border-left-color", await resolvedColor(page, "--color-session-active"));
  });
});

test.describe("sidebar Groups (#1234)", () => {
  test("Groups renders user groups by group_path; the pick and their collapse persist across reload", async ({
    page,
  }) => {
    const mocks = await installSidebarMocks(page, { sessions: groupedSessions() });
    await gotoDesktop(page);

    const headers = page.locator(HEADER);
    // One repository header with the two Groups nested inside it.
    await expect(headers).toHaveCount(3);
    await expect(page.locator(ROW)).toHaveCount(3);

    const viewToggle = page.locator(VIEW_TOGGLE);
    await expect(viewToggle).toHaveAttribute("data-group-by", "project");
    await pickGrouping(page, viewToggle, "group");
    await expect(viewToggle).toHaveAttribute("data-sort", "custom");
    expect(mocks.viewPatches).toEqual([{ group_by: "group" }]);

    await expect(headers).toHaveCount(2);
    await expect(page.locator(`${HEADER}[data-group-id='feature']`)).toBeVisible();
    await expect(page.locator(`${HEADER}[data-group-id='refactor']`)).toBeVisible();
    await expect(page.locator(ROW)).toHaveCount(3);

    const featureHeader = page.locator(`${HEADER}[data-group-id='feature']`);
    const featureExpand = featureHeader.locator("button[aria-expanded]");
    await expect(featureExpand).toHaveAttribute("aria-expanded", "true");

    await featureExpand.click();
    await expect(featureExpand).toHaveAttribute("aria-expanded", "false");
    await expect(page.getByText("feat-one")).toBeHidden();

    // The grouping comes back from the Daemon, the collapse from this browser.
    await page.reload();
    await expect(viewToggle).toHaveAttribute("data-group-by", "group");
    await expect(featureHeader.locator("button[aria-expanded]")).toHaveAttribute("aria-expanded", "false");

    // Collapse maps are per grouping.
    await pickGrouping(page, viewToggle, "project");
    for (const expand of await page.locator(`${HEADER} button[aria-expanded]`).all()) {
      await expect(expand).toHaveAttribute("aria-expanded", "true");
    }
  });
});

test.describe("sidebar group header + (#5498)", () => {
  test("opens the wizard on the remembered project with that group, and creates the session in it", async ({
    page,
  }) => {
    const created = await mockWizardApis(page, {
      projects: [],
      sessions: [sessionStub({ id: "s-f1", title: "feat-one", group_path: "feature" })],
    });
    await page.addInitScript(() => localStorage.setItem("aoe-new-session-last-project", "/tmp/remembered"));
    await gotoDesktop(page);
    await pickGrouping(page, page.locator(VIEW_TOGGLE), "group");

    await page
      .locator(`${HEADER}[data-group-id='feature']`)
      .getByRole("button", { name: "New session in feature" })
      .click();

    const w = wizard(page);
    await expect(w.getByTestId("wizard-project-row")).toContainText("/tmp/remembered");
    await launch(page);
    await expect.poll(() => created.length).toBe(1);
    expect(created[0]).toMatchObject({ path: "/tmp/remembered", group: "feature" });
  });
});

test.describe("sidebar Repository grouping (#1720)", () => {
  test("nests user groups inside the repo block; subgroup collapse is independent of repo collapse and persists", async ({
    page,
  }) => {
    await installSidebarMocks(page, { sessions: nestedSessions() });
    await gotoDesktop(page);

    const repoBlocks = page.locator("[data-testid='sidebar-nested-repo']");
    await expect(repoBlocks).toHaveCount(1);
    const repo = repoBlocks.first();

    await expect(repo.locator("[data-testid='sidebar-nested-subgroup']")).toHaveCount(2);
    await expect(repo.locator("[data-testid='sidebar-nested-subgroup'] [data-group-id='feature']")).toBeVisible();
    await expect(repo.locator("[data-testid='sidebar-nested-subgroup'] [data-group-id='fix']")).toBeVisible();
    // A session in no Group sits straight under the repository header, above the Groups.
    await expect(repo.locator("[data-group-id='__ungrouped__']")).toHaveCount(0);
    await expect(repo.locator(ROW).first()).toContainText("loose-one");

    await expect(page.locator(ROW)).toHaveCount(4);

    const featureSub = page.locator("[data-testid='sidebar-nested-subgroup'] [data-group-id='feature']");
    const featureExpand = featureSub.locator("button[aria-expanded]");
    await expect(featureExpand).toHaveAttribute("aria-expanded", "true");

    await featureExpand.click();
    await expect(featureExpand).toHaveAttribute("aria-expanded", "false");
    await expect(page.getByText("feat-one")).toBeHidden();
    await expect(page.getByText("fix-one")).toBeVisible();

    await page.reload();
    await expect(
      page
        .locator("[data-testid='sidebar-nested-subgroup'] [data-group-id='feature']")
        .locator("button[aria-expanded]"),
    ).toHaveAttribute("aria-expanded", "false");

    const repoHeader = page.locator("[data-testid='sidebar-nested-repo']").first().locator(HEADER).first();
    await repoHeader.locator("button[aria-expanded]").click();
    await expect(page.locator("[data-testid='sidebar-nested-subgroup']")).toHaveCount(0);
  });

  test("a Shift+click range skips the rows hidden under a collapsed repo", async ({ page }) => {
    const sessions: MockSessionInput[] = ["a", "b", "c"].map((name) => ({
      id: `s-${name}`,
      title: `${name}-session`,
      project_path: `/tmp/repo-${name}`,
      branch: `feat/${name}`,
      group: "feature",
    }));
    const archived: string[] = [];
    await installSidebarMocks(page, { sessions });
    await recordArchives(page, archived, sessions);
    await gotoDesktop(page);

    // Hide repo B's row, then range from A to C across it.
    const repoB = page.locator("[data-testid='sidebar-nested-repo'][data-repo-id='/tmp/repo-b']");
    await repoB.locator(HEADER).first().locator("button[aria-expanded]").click();
    await expect(page.getByText("b-session")).toBeHidden();

    const row = (title: string) => page.locator(ROW).filter({ hasText: title });
    await row("a-session").click({ modifiers: ["Meta"] });
    await row("c-session").click({ modifiers: ["Shift"] });
    await row("c-session").click({ button: "right" });
    await page.getByTestId("sidebar-context-menu-bulk-archive").click();

    await expect.poll(() => archived.length).toBe(2);
    expect([...archived].sort()).toEqual(["s-a", "s-c"]);
  });
});

test.describe("sidebar view menu and Groups slices (#5896)", () => {
  test("the view menu follows the menu-button pattern: focus on open, arrows, Escape and Tab-out", async ({ page }) => {
    await installSidebarMocks(page, { sessions: twoRepoSessions() });
    await gotoDesktop(page);

    const viewToggle = page.locator(VIEW_TOGGLE);
    const menu = page.getByTestId("sidebar-view-menu");
    const item = (testId: string) => page.getByTestId(testId);
    await viewToggle.focus();
    await page.keyboard.press("Enter");

    await expect(menu).toBeVisible();
    await expect(menu).toHaveAttribute("role", "menu");
    await expect(item("sidebar-view-group-project")).toHaveAttribute("role", "menuitemradio");
    // Focus enters on the checked grouping, and the menu owns the arrows, so items are not Tab stops.
    await expect(item("sidebar-view-group-project")).toBeFocused();
    await expect(item("sidebar-view-group-project")).toHaveAttribute("tabindex", "-1");

    // One View options menu holds grouping, sorting and the filter; the arrows walk all of it.
    await page.keyboard.press("ArrowDown");
    await expect(item("sidebar-view-group-group")).toBeFocused();
    await page.keyboard.press("End");
    await expect(item("sidebar-filter-toggle")).toBeFocused();
    // Both ends wrap.
    await page.keyboard.press("ArrowDown");
    await expect(item("sidebar-view-group-project")).toBeFocused();
    await page.keyboard.press("ArrowUp");
    await expect(item("sidebar-filter-toggle")).toBeFocused();
    await page.keyboard.press("Home");
    await expect(item("sidebar-view-group-project")).toBeFocused();

    // Escape closes without picking and hands the focus back to the trigger.
    await page.keyboard.press("Escape");
    await expect(menu).toBeHidden();
    await expect(viewToggle).toBeFocused();
    await expect(viewToggle).toHaveAttribute("data-group-by", "project");

    // Enter on an arrowed-to item commits it, and the trigger keeps the focus.
    await page.keyboard.press("Enter");
    await page.keyboard.press("ArrowDown");
    await page.keyboard.press("Enter");
    await expect(menu).toBeHidden();
    await expect(viewToggle).toHaveAttribute("data-group-by", "group");
    await expect(viewToggle).toBeFocused();

    // Tabbing out closes the menu instead of walking its options.
    await page.keyboard.press("Enter");
    await expect(menu).toBeVisible();
    await page.keyboard.press("Tab");
    await expect(menu).toBeHidden();
  });

  test("bulk archive targets each selected row's own session when one workspace straddles two Groups", async ({
    page,
  }) => {
    // One workspace with a session in each of two Groups: Groups renders it twice, and the
    // alpha Group sorts first, so the workspace id alone resolves to its session.
    const sessions: MockSessionInput[] = [
      {
        id: "s-wait",
        group: "alpha",
        title: "wait-session",
        project_path: "/tmp/shared",
        branch: "feat/shared",
        fields: { status: "Waiting" },
      },
      {
        id: "s-run",
        group: "beta",
        title: "run-session",
        project_path: "/tmp/shared",
        branch: "feat/shared",
        fields: { status: "Running" },
      },
      {
        id: "s-other",
        title: "other-session",
        project_path: "/tmp/other",
        branch: "feat/other",
        fields: { status: "Running" },
      },
    ];
    const archived: string[] = [];
    await installSidebarMocks(page, { sessions });
    await recordArchives(page, archived, sessions);
    await gotoDesktop(page);
    await pickGrouping(page, page.locator(VIEW_TOGGLE), "group");

    await expect(page.getByText("wait-session")).toBeVisible();
    // Cmd rather than Ctrl: Chromium on macOS reads Ctrl+click as a secondary click.
    const [runRow, otherRow] = ["run-session", "other-session"].map((title) =>
      page.locator(ROW).filter({ hasText: title }),
    );
    await runRow!.click({ modifiers: ["Meta"] });
    await otherRow!.click({ modifiers: ["Meta"] });
    await otherRow!.click({ button: "right" });
    await page.getByTestId("sidebar-context-menu-bulk-archive").click();

    await expect.poll(() => archived.length).toBe(2);
    expect([...archived].sort()).toEqual(["s-other", "s-run"]);
  });

  test("a workspace split across Groups marks only the slice showing the active session", async ({ page }) => {
    const sessions: MockSessionInput[] = [
      {
        id: "s-wait",
        group: "alpha",
        title: "wait-session",
        project_path: "/tmp/shared",
        branch: "feat/shared",
        fields: { status: "Waiting" },
      },
      {
        id: "s-run",
        group: "beta",
        title: "run-session",
        project_path: "/tmp/shared",
        branch: "feat/shared",
        fields: { status: "Running" },
      },
    ];
    await installSidebarMocks(page, { sessions });
    await gotoDesktop(page);
    await pickGrouping(page, page.locator(VIEW_TOGGLE), "group");

    await page.locator(ROW).filter({ hasText: "run-session" }).click();
    await expect(page).toHaveURL(/\/session\/s-run/);
    // Both slices share the workspace id, but only the beta one shows the open session.
    const active = page.locator("[data-active-session-row]");
    await expect(active).toHaveCount(1);
    await expect(active).toContainText("run-session");
  });

  test("a range across the footer selects each workspace once, not its hidden Group slices", async ({ page }) => {
    const archivedUntil = { archived_at: "2026-01-01T00:00:00Z" };
    const sessions: MockSessionInput[] = [
      {
        id: "s-w1-waiting",
        group: "alpha",
        title: "w1-waiting",
        project_path: "/tmp/w1",
        branch: "feat/w1",
        fields: { status: "Waiting", ...archivedUntil },
      },
      {
        id: "s-w1-running",
        group: "beta",
        title: "w1-running",
        project_path: "/tmp/w1",
        branch: "feat/w1",
        fields: { status: "Running", ...archivedUntil },
      },
      {
        id: "s-w2-idle",
        group: "alpha",
        title: "w2-idle",
        project_path: "/tmp/w2",
        branch: "feat/w2",
        fields: { status: "Idle", ...archivedUntil },
      },
    ];
    const unarchived: string[] = [];
    await installSidebarMocks(page, { sessions });
    await recordArchives(page, unarchived);
    await gotoDesktop(page);
    await pickGrouping(page, page.locator(VIEW_TOGGLE), "group");
    await page.getByTestId("sidebar-sunk-toggle").click();

    // One footer row per workspace: W1 is sliced into two Groups internally.
    const footerRows = page.locator(`[data-testid='sidebar-sunk-section'] ${ROW}`);
    await expect(footerRows).toHaveCount(2);

    await footerRows.nth(0).click();
    await footerRows.nth(1).click({ modifiers: ["Shift"] });
    await footerRows.nth(1).click({ button: "right" });
    await page.getByTestId("sidebar-context-menu-bulk-unarchive").click();

    await expect.poll(() => unarchived.length).toBe(2);
    expect([...unarchived].sort()).toEqual(["s-w1-waiting", "s-w2-idle"]);
  });

  test("snoozing one Group slice leaves its sibling row and bulk eligibility alone", async ({ page }) => {
    const sessions: MockSessionInput[] = [
      {
        id: "s-wait",
        group: "alpha",
        title: "wait-session",
        project_path: "/tmp/shared",
        branch: "feat/shared",
        fields: { status: "Waiting" },
      },
      {
        id: "s-run",
        group: "beta",
        title: "run-session",
        project_path: "/tmp/shared",
        branch: "feat/shared",
        fields: { status: "Running" },
      },
    ];
    const snoozed: string[] = [];
    await installSidebarMocks(page, { sessions });
    await page.route("**/api/sessions/*/snooze", async (route) => {
      const id = decodeURIComponent(new URL(route.request().url()).pathname.split("/").at(-2)!);
      snoozed.push(id);
      const { minutes } = route.request().postDataJSON() as { minutes: number | null };
      await route.fulfill({
        json: sessionResponse({ id, snoozed_until: minutes == null ? null : "2099-01-01T00:00:00Z" }),
      });
    });
    await gotoDesktop(page);
    await pickGrouping(page, page.locator(VIEW_TOGGLE), "group");

    const waitingRow = page.locator(ROW).filter({ hasText: "wait-session" });
    const runningRow = page.locator(ROW).filter({ hasText: "run-session" });
    await runningRow.click({ button: "right" });
    await page.getByTestId("sidebar-context-menu-snooze").click();
    await page.getByTestId("snooze-modal-preset-60").click();
    await expect.poll(() => snoozed).toEqual(["s-run"]);

    // Only the beta row carries the pending snooze.
    await expect(runningRow.getByLabel("Snoozed")).toBeVisible();
    await expect(waitingRow.getByLabel("Snoozed")).toHaveCount(0);

    // Bulk buckets follow the same scope: one snoozable row, one unsnoozable. A plain click first
    // clears the single selection the snooze row's right-click left behind.
    await waitingRow.click();
    await waitingRow.click({ modifiers: ["Meta"] });
    await runningRow.click({ modifiers: ["Meta"] });
    await runningRow.click({ button: "right" });
    await expect(page.getByTestId("sidebar-context-menu-bulk-unsnooze")).toHaveText("Unsnooze 1");
    await expect(page.getByText("Snooze 1", { exact: true })).toBeVisible();
  });

  test("archiving one Group slice leaves its sibling slice's own pending action alone", async ({ page }) => {
    const sessions: MockSessionInput[] = [
      {
        id: "s-wait",
        group: "alpha",
        title: "wait-session",
        project_path: "/tmp/shared",
        branch: "feat/shared",
        fields: { status: "Waiting" },
      },
      {
        id: "s-run",
        group: "beta",
        title: "run-session",
        project_path: "/tmp/shared",
        branch: "feat/shared",
        fields: { status: "Running" },
      },
    ];
    const patched: string[] = [];
    let release!: () => void;
    const held = new Promise<void>((resolve) => (release = resolve));
    await installSidebarMocks(page, { sessions });
    // Hold both PATCHes so the two slices carry pending values at the same time.
    const hold = async (route: Route) => {
      const parts = new URL(route.request().url()).pathname.split("/");
      const id = decodeURIComponent(parts.at(-2)!);
      patched.push(`${id}:${parts.at(-1)}`);
      await held;
      await route.fulfill({ json: sessionResponse({ id }) });
    };
    await page.route("**/api/sessions/*/archive", hold);
    await page.route("**/api/sessions/*/unread", hold);
    await gotoDesktop(page);
    await pickGrouping(page, page.locator(VIEW_TOGGLE), "group");

    const row = (title: string) => page.locator(ROW).filter({ hasText: title });
    await row("wait-session").click({ button: "right" });
    await page.getByTestId("sidebar-context-menu-archive").click();
    await expect(row("wait-session").getByTitle("Archived")).toBeVisible();

    // The sibling's action must not claim the alpha slice's pending archive.
    await row("run-session").click({ button: "right" });
    await page.getByTestId("sidebar-context-menu-unread").click();
    await expect.poll(() => patched.length).toBe(2);

    await expect(row("wait-session").getByTitle("Archived")).toBeVisible();
    await expect(row("run-session").getByTitle("Archived")).toHaveCount(0);

    // Bulk eligibility follows each slice's own state: one pending-archived, one live.
    await row("wait-session").click();
    await row("wait-session").click({ modifiers: ["Meta"] });
    await row("run-session").click({ modifiers: ["Meta"] });
    await row("run-session").click({ button: "right" });
    await expect(page.getByTestId("sidebar-context-menu-bulk-unarchive")).toHaveText("Unarchive 1");
    await expect(page.getByTestId("sidebar-context-menu-bulk-archive")).toHaveText("Archive 1");

    release();
    await expect.poll(() => [...patched].sort()).toEqual(["s-run:unread", "s-wait:archive"]);
  });
});
