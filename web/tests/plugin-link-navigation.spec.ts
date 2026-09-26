// #4089: plugin UI links may target aoe's own origin (relative paths, since a
// plugin cannot know aoe's real host). Those navigate via the SPA router
// instead of opening a new tab, and a session-shaped link goes through the
// same session-selection flow a sidebar click uses (not a bare route change).
// Everything else (external links, modified clicks) keeps native anchor
// behavior. Href classification itself (relative vs. scheme-relative vs.
// same-origin absolute) is unit-tested in src/lib/__tests__/pluginHref.test.ts.

import { test, expect } from "./helpers/mockedTest";
import { mockStaticApis, mockSessionShellApis } from "./helpers/apiMocks";
import { openMobileSidebar } from "./helpers/sidebar";
import { sessionResponse } from "./helpers/sessions";
import type { Page } from "@playwright/test";

async function mockApis(page: Page, uiEntries: unknown[]) {
  await mockStaticApis(page);
  await mockSessionShellApis(page);
  await page.route("**/api/sessions", (r) => {
    if (r.request().method() !== "GET") return r.fulfill({ status: 400 });
    return r.fulfill({
      json: {
        sessions: [
          sessionResponse({ id: "sess-a", title: "Ethiopians", project_path: "/tmp/agent-of-empires" }),
          sessionResponse({ id: "sess-b", title: "Celts", project_path: "/tmp/agent-of-empires" }),
        ],
        workspace_ordering: [],
      },
    });
  });
  await page.route("**/api/plugins/ui-state", (r) => r.fulfill({ json: { entries: uiEntries, notifications: [] } }));
}

// Three row-badges on sess-a's row: an internal session link, an internal
// non-session link, and an external link.
const UI_ENTRIES = [
  {
    plugin_id: "acme.kit",
    slot: "row-badge",
    id: "session-link",
    session_id: "sess-a",
    payload: { text: "acme: session link", href: "/session/sess-b" },
  },
  {
    plugin_id: "acme.kit",
    slot: "row-badge",
    id: "settings-link",
    session_id: "sess-a",
    payload: { text: "acme: settings link", href: "/settings" },
  },
  {
    plugin_id: "acme.kit",
    slot: "row-badge",
    id: "external-link",
    session_id: "sess-a",
    payload: { text: "acme: pr link", href: "https://github.com/o/r/pull/1" },
  },
];

test.describe("Plugin UI link navigation (#4089)", () => {
  test("an external link opens a new tab; an internal relative link routes without a page load", async ({ page }) => {
    await mockApis(page, UI_ENTRIES);
    let documentRequests = 0;
    page.on("request", (request) => {
      if (request.resourceType() === "document" && new URL(request.url()).pathname === "/settings") {
        documentRequests += 1;
      }
    });
    await page.setViewportSize({ width: 1280, height: 800 });
    await page.goto("/");
    // exact: true: a badge's name is otherwise a substring of its parent
    // sidebar row's own accessible name (which concatenates all its badges).
    const settingsLink = page.getByRole("link", { name: "acme: settings link", exact: true });

    const [external] = await Promise.all([
      page.context().waitForEvent("page"),
      page.getByRole("link", { name: "acme: pr link", exact: true }).click(),
    ]);
    await expect(external).toHaveURL("https://github.com/o/r/pull/1");
    await expect(page).toHaveURL("/");

    await settingsLink.click();
    await expect(page).toHaveURL(/\/settings$/);
    expect(documentRequests).toBe(0);
  });

  test("a session-shaped internal link goes through session selection, not a bare route change", async ({ page }) => {
    await mockApis(page, UI_ENTRIES);
    await page.setViewportSize({ width: 390, height: 844 });
    await page.goto("/");
    // handleSelectSession closes the mobile sidebar on selection; a bare
    // route change would leave it open. Open it first so that's observable.
    await openMobileSidebar(page);

    await page.getByRole("link", { name: "acme: session link", exact: true }).click();

    await expect(page).toHaveURL(/\/session\/sess-b$/);
    await expect
      .poll(() =>
        page.evaluate(() => {
          const r = document.querySelector('[data-testid="sidebar-session-row"]')?.getBoundingClientRect();
          return r ? r.x < 0 : true;
        }),
      )
      .toBe(true);
  });

  test("a modified click on an internal link is not intercepted", async ({ page }) => {
    await mockApis(page, UI_ENTRIES);
    await page.setViewportSize({ width: 1280, height: 800 });
    await page.goto("/");

    const [popup] = await Promise.all([
      page.context().waitForEvent("page"),
      page
        .getByRole("link", { name: "acme: settings link", exact: true })
        .click({ modifiers: [process.platform === "darwin" ? "Meta" : "Control"] }),
    ]);
    await expect(popup).toHaveURL(/\/settings$/);
    // The click handler bailed out on the modifier key instead of calling
    // preventDefault + router navigation, so the original tab never moved.
    await expect(page).toHaveURL("/");
  });
});
