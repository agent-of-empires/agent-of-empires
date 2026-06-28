// Mocked-Playwright coverage for the plugin row slots on a narrow mobile
// sidebar (#2514).
//
// The github plugin's row-badge (icon chips) and row-column (status text) used
// to render inline on the session-name line. On the narrow mobile drawer the
// truncating name kept its width, so the column squeezed to zero and the
// shrink-0 badges overflowed past the row's right edge. They now sit on their
// own line (PluginRowLine), so both stay within the row regardless of how long
// the session name is. This drives the real-CSS layout at a mobile viewport,
// which jsdom cannot reproduce.

import { test, expect } from "./helpers/mockedTest";
import { Page } from "@playwright/test";

const LONG_TITLE = "this-is-a-deliberately-very-long-session-name-that-eats-the-whole-row-width-on-mobile";

function sessionResponse() {
  return {
    id: "s1",
    title: LONG_TITLE,
    project_path: "/tmp/repo",
    group_path: "/tmp/repo",
    tool: "claude",
    status: "Idle",
    yolo_mode: false,
    created_at: "2025-01-01T00:00:00Z",
    last_accessed_at: null,
    idle_entered_at: null,
    last_error: null,
    branch: "feature/x",
    main_repo_path: null,
    is_sandboxed: false,
    favorited: false,
    urgent: false,
    has_terminal: true,
    profile: "default",
    workspace_repos: [],
  };
}

// One icon chip per repo across a multi-repo workspace: enough shrink-0 badges
// that the old inline layout overflowed the narrow row instead of wrapping.
const BADGE_ITEMS = Array.from({ length: 12 }, (_, i) => ({
  icon: "git-pull-request",
  tone: "success",
  tooltip: `PR #${i + 1}`,
}));

const UI_ENTRIES = [
  {
    plugin_id: "acme.kit",
    slot: "row-badge",
    id: "github_pr_badge",
    session_id: "s1",
    payload: { items: BADGE_ITEMS },
  },
  {
    plugin_id: "acme.kit",
    slot: "row-column",
    id: "github_pr_status",
    session_id: "s1",
    payload: { text: "Changes requested", tone: "warning" },
  },
];

async function mockApis(page: Page) {
  await page.route("**/api/login/status", (r) => r.fulfill({ json: { required: false, authenticated: true } }));
  await page.route("**/api/sessions", (r) => {
    if (r.request().method() !== "GET") return r.fulfill({ status: 400 });
    return r.fulfill({
      json: { sessions: [sessionResponse()], workspace_ordering: ["/tmp/repo::feature/x"] },
    });
  });
  await page.route("**/api/plugins/ui-state", (r) => r.fulfill({ json: { entries: UI_ENTRIES, notifications: [] } }));
  for (const path of ["settings", "themes", "agents", "profiles", "groups", "devices", "docker/status", "about"]) {
    await page.route(`**/api/${path}`, (r) => r.fulfill({ json: path === "docker/status" ? {} : [] }));
  }
}

// A child element is "within" the row when its right edge does not spill past
// the row's right edge (a couple of px of slack for sub-pixel rounding).
async function allWithinRow(page: Page, selector: string): Promise<boolean> {
  return page.evaluate((sel) => {
    const row = document.querySelector("[data-testid='sidebar-session-row']");
    const els = Array.from(document.querySelectorAll(sel));
    if (!row || els.length === 0) return false;
    const r = row.getBoundingClientRect();
    return els.every((el) => {
      const e = el.getBoundingClientRect();
      return e.width > 0 && e.right <= r.right + 2;
    });
  }, selector);
}

test.describe("Plugin row slots on a mobile sidebar (#2514)", () => {
  test("row-column and badges stay within the row next to a long name", async ({ page }) => {
    await page.setViewportSize({ width: 390, height: 844 });
    await mockApis(page);
    await page.goto("/");

    // The drawer starts closed on mobile; open it.
    await page.getByRole("button", { name: "Toggle sidebar" }).click();
    await expect(page.locator("[data-testid='sidebar-session-row']")).toHaveCount(1, { timeout: 8000 });

    const column = "[data-plugin-slot='row-column']";
    const badge = "[data-plugin-slot='row-badge']";
    await expect(page.locator(column)).toBeVisible();
    await expect(page.locator(badge).first()).toBeVisible();

    // The status text and every badge icon render inside the row, not squeezed
    // to zero or clipped past its right edge.
    expect(await allWithinRow(page, column)).toBe(true);
    expect(await allWithinRow(page, badge)).toBe(true);
  });
});
