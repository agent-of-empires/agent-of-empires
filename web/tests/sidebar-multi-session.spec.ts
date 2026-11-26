import { test, expect } from "./helpers/mockedTest";
import { mockStaticApis } from "./helpers/apiMocks";
import { sessionResponse } from "./helpers/sessions";
import { Page } from "@playwright/test";

// Two sessions sharing the same `(project_path, branch=null)` collapsed
// behind `workspace.sessions[0]` and only one rendered in the sidebar.
// useWorkspaces now splits null-branch sessions into one workspace per
// session so both rows appear. See #956.

interface MockSession {
  id: string;
  title: string;
  project_path: string;
  branch: string | null;
  status?: string;
}

const ROW_TAG_SCHEMA = [
  {
    section: "session",
    field: "row_tag",
    category: "Sessions",
    label: "Row Tag",
    description: "What to show next to each session title",
    widget: {
      kind: "select",
      options: [
        { value: "none", label: "None" },
        { value: "auto", label: "Auto" },
        { value: "profile", label: "Profile" },
        { value: "sandbox", label: "Sandbox" },
        { value: "branch", label: "Branch" },
      ],
    },
    web_write: { policy: "allow" },
    profile_overridable: true,
    validation: { rule: "none" },
    advanced: false,
  },
];

async function mockApis(
  page: Page,
  sessions: MockSession[],
  options: { rowTag?: "none" | "auto" | "profile" | "sandbox" | "branch" } = {},
) {
  let rowTag = options.rowTag ?? "branch";
  await mockStaticApis(page, { profiles: [{ name: "default", is_default: true }] });
  await page.route("**/api/settings**", (r) => {
    const pathname = new URL(r.request().url()).pathname;
    if (pathname === "/api/settings/schema") return r.fulfill({ json: ROW_TAG_SCHEMA });
    if (pathname === "/api/settings") {
      if (r.request().method() === "GET") return r.fulfill({ json: { session: { row_tag: rowTag } } });
      if (r.request().method() === "PATCH") {
        const body = r.request().postDataJSON() as { session?: { row_tag?: typeof rowTag } };
        rowTag = body.session?.row_tag ?? rowTag;
        return r.fulfill({ json: { ok: true } });
      }
    }
    return r.fulfill({ status: 404 });
  });
  await page.route(/\/api\/sessions(\?.*)?$/, (r) => {
    if (r.request().method() !== "GET") return r.fulfill({ status: 400 });
    return r.fulfill({
      headers: { "aoe-runtime-epoch": "mock-runtime", "aoe-runtime-revision": "1" },
      json: {
        sessions: sessions.map((s) => sessionResponse({ ...s })),
        workspace_ordering: [],
      },
    });
  });
}

test.describe("Sidebar multi-session (#956)", () => {
  test("a deliberate desktop click navigates client-side and is not swallowed as a drag", async ({ page }) => {
    await mockApis(page, [
      {
        id: "sess-a",
        title: "Ethiopians",
        project_path: "/tmp/agent-of-empires",
        branch: null,
      },
      {
        id: "sess-b",
        title: "Celts",
        project_path: "/tmp/agent-of-empires",
        branch: null,
      },
    ]);

    let sessionDocumentRequests = 0;
    page.on("request", (request) => {
      if (request.resourceType() === "document" && /\/session\/sess-a$/.test(new URL(request.url()).pathname)) {
        sessionDocumentRequests += 1;
      }
    });

    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/session/sess-b");
    await expect(page.locator("header")).toBeVisible();
    await expect(page).toHaveURL(/\/session\/sess-b$/);

    const row = page.getByRole("link", { name: /Ethiopians/i });
    await expect(row).toHaveJSProperty("tagName", "A");
    await expect(row).toHaveAttribute("href", /\/session\/sess-a$/);
    const box = await row.boundingBox();
    expect(box).not.toBeNull();

    await page.mouse.move(box!.x + box!.width / 2, box!.y + box!.height / 2);
    await page.mouse.down();
    await page.waitForTimeout(220);
    await page.mouse.up();

    await expect(row).toHaveAttribute("data-active-session-row", "true");
    // The open row reads as a filled pill: its background differs from the sidebar's.
    const fill = await row.evaluate((el) => getComputedStyle(el).backgroundColor);
    expect(fill).not.toBe("rgba(0, 0, 0, 0)");
    await expect(page).toHaveURL(/\/session\/sess-a$/);
    expect(sessionDocumentRequests).toBe(0);
  });

  test("deleting rows are disabled for pointer and keyboard activation", async ({ page }) => {
    await mockApis(page, [
      {
        id: "sess-a",
        title: "Ethiopians",
        project_path: "/tmp/agent-of-empires",
        branch: null,
        status: "Deleting",
      },
      {
        id: "sess-b",
        title: "Celts",
        project_path: "/tmp/agent-of-empires",
        branch: null,
      },
    ]);

    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/session/sess-b");
    await expect(page.locator("header")).toBeVisible();
    const row = page.getByRole("link", { name: /Ethiopians/i });

    await expect(row).toHaveAttribute("aria-disabled", "true");
    await expect(row).toHaveAttribute("tabindex", "-1");

    await row.evaluate((el) => (el as HTMLElement).click());
    await expect(page).toHaveURL(/\/session\/sess-b$/);

    await row.evaluate((el) => {
      el.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true }));
    });
    await expect(page).toHaveURL(/\/session\/sess-b$/);
  });

  test("saving row tag settings refreshes the sidebar suffix", async ({ page }) => {
    await mockApis(page, [
      {
        id: "sess-a",
        title: "Italians",
        project_path: "/tmp/agent-of-empires",
        branch: "feature/a",
      },
    ]);

    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/");
    await expect(page.locator("header")).toBeVisible();
    const row = page.getByRole("link", { name: /Italians/i });
    await expect(row.getByTestId("sidebar-session-row-tag")).toHaveText("[a]");

    await page.goto("/settings/session");
    const rowTagSelect = page
      .locator("label", { hasText: /^Row Tag$/ })
      .locator("..")
      .getByRole("combobox");
    await expect(rowTagSelect).toHaveValue("branch");
    const savedSettings = page.waitForResponse((response) => {
      const request = response.request();
      const url = new URL(response.url());
      return (
        request.method() === "PATCH" &&
        url.pathname === "/api/settings" &&
        url.searchParams.get("profile") === "default"
      );
    });
    const refreshedSettings = page.waitForResponse(async (response) => {
      const url = new URL(response.url());
      if (response.request().method() !== "GET" || url.pathname !== "/api/settings" || url.search) return false;
      const settings = (await response.json()) as { session?: { row_tag?: string } };
      return settings.session?.row_tag === "none";
    });
    await rowTagSelect.selectOption("none");
    const saved = await savedSettings;
    expect(saved.ok()).toBe(true);
    expect(saved.request().postDataJSON()).toEqual({ session: { row_tag: "none" } });
    await refreshedSettings;
    await page.getByRole("button", { name: "Go to dashboard" }).click();
    await expect(row).toBeVisible();
    await expect(row.getByTestId("sidebar-session-row-tag")).toHaveCount(0);
  });

  test("project group context menu archives every active session", async ({ page }) => {
    await mockApis(page, [
      {
        id: "sess-a",
        title: "Ethiopians",
        project_path: "/tmp/agent-of-empires",
        branch: null,
      },
      {
        id: "sess-b",
        title: "Celts",
        project_path: "/tmp/agent-of-empires",
        branch: null,
      },
    ]);

    const archived: string[] = [];
    await page.route("**/api/sessions/*/archive", async (r) => {
      const url = new URL(r.request().url());
      const id = url.pathname.split("/")[3];
      const body = r.request().postDataJSON() as { archived: boolean };
      if (body.archived) archived.push(id);
      await r.fulfill({ json: { id, archived_at: new Date().toISOString() } });
    });

    // The "archive all in project" action confirms first; accept it.
    let confirmText = "";
    page.on("dialog", (dialog) => {
      confirmText = dialog.message();
      void dialog.accept();
    });

    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/");
    await expect(page.locator("header")).toBeVisible();

    const projectHeader = page.locator('[data-testid="sidebar-group-header"][data-group-id="/tmp/agent-of-empires"]');
    await projectHeader.click({ button: "right" });
    const menu = page.locator("[data-testid='sidebar-group-context-menu']");
    await expect(menu).toBeVisible();

    const archiveAll = menu.locator("[data-testid='sidebar-group-context-menu-archive-all']");
    await expect(archiveAll).toHaveText("Archive all (2)");
    await archiveAll.click();

    await expect.poll(() => archived.slice().sort()).toEqual(["sess-a", "sess-b"]);
    expect(confirmText).toContain("Archive all 2 sessions");
  });
});

// Sessions sharing an explicit worktree branch stay in one row; its session actions must name a sibling (#3387).
test.describe("Sidebar shared-worktree row actions (#3387)", () => {
  const shared = { project_path: "/tmp/agent-of-empires", branch: "feature/shared" };

  test("Stop and Start act on the chosen sibling, never the first session", async ({ page }) => {
    await mockApis(page, [
      { id: "sess-a", title: "Creator", status: "Stopped", ...shared },
      { id: "sess-b", title: "Reviewer", status: "Running", ...shared },
      { id: "sess-c", title: "Security", status: "Running", ...shared },
    ]);
    const lifecycle: string[] = [];
    await page.route(/\/api\/sessions\/[^/]+\/(stop|start)$/, (r) => {
      const [, id, verb] = new URL(r.request().url()).pathname.match(/sessions\/([^/]+)\/(\w+)$/)!;
      lifecycle.push(`${verb} ${id}`);
      return r.fulfill({ json: { id } });
    });
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/");

    const row = page.getByTestId("sidebar-session-row");
    await expect(row).toHaveCount(1, { timeout: 10_000 });
    await row.click({ button: "right" });
    await page.getByTestId("sidebar-context-menu-start").click();
    await expect.poll(() => lifecycle, { timeout: 10_000 }).toEqual(["start sess-a"]);

    await row.click({ button: "right" });
    await page.getByTestId("sidebar-context-menu-stop").click();
    const targets = page.getByTestId("sidebar-context-menu-stop-target");
    await expect(targets).toHaveText([/Reviewer/, /Security/]);
    await targets.nth(1).click();
    await page
      .getByTestId("stop-session-dialog")
      .getByRole("button", { name: /^Stop$/ })
      .click();
    await expect.poll(() => lifecycle, { timeout: 10_000 }).toEqual(["start sess-a", "stop sess-c"]);
  });
});
