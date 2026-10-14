// #4460: plugin pages open full width from the TopBar or palette, framed with
// `sandbox="allow-scripts"` so plugin script never runs on the dashboard origin.

import { test, expect } from "./helpers/mockedTest";
import { mockStaticApis, mockSessionShellApis } from "./helpers/apiMocks";
import { sessionResponse } from "./helpers/sessions";
import type { Page } from "@playwright/test";

const PAGE = {
  plugin_id: "acme.kit",
  id: "board",
  title: "Board",
  icon: "kanban",
  url: "/api/plugins/acme.kit/pages/board",
};

// The daemon's page policy (src/server/api/plugins.rs `PLUGIN_PAGE_CSP`).
const PAGE_CSP =
  "sandbox allow-scripts; default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; " +
  "img-src data: blob:; connect-src 'none'; form-action 'none'; base-uri 'none'; frame-ancestors 'self'";

// Reports what the page could reach back to the dashboard.
const FIXTURE = `<!doctype html><p>board fixture</p><script>
const probe = (f) => { try { return String(f()); } catch (e) { return "blocked:" + e.name; } };
const report = { storage: probe(() => localStorage.getItem("aoe_auth_token")), cookie: probe(() => document.cookie), href: location.href };
fetch("/api/sessions").then(() => "reached", (e) => "blocked:" + e.name).then((api) => parent.postMessage({ ...report, api }, "*"));
</script>`;

async function mockApis(page: Page, pages: unknown[]) {
  await mockStaticApis(page);
  await mockSessionShellApis(page);
  await page.route("**/api/sessions", (r) =>
    r.fulfill({
      json: {
        sessions: [sessionResponse({ id: "sess-a", title: "Ethiopians", project_path: "/tmp/agent-of-empires" })],
        workspace_ordering: [],
      },
    }),
  );
  await page.route("**/api/plugins/ui-state", (r) => r.fulfill({ json: { entries: [], notifications: [], pages } }));
  await page.route(`**${PAGE.url}/ticket`, (r) => r.fulfill({ status: 204 }));
  await page.route(`**${PAGE.url}`, (r) =>
    r.fulfill({ contentType: "text/html", headers: { "content-security-policy": PAGE_CSP }, body: FIXTURE }),
  );
}

/** Opens the palette once the first ui-state poll has landed, so a missing entry is not just an early read.
 *  Starts on a session, where no first-run overlay covers the palette. */
async function searchPalette(page: Page, query: string) {
  const polled = page.waitForResponse("**/api/plugins/ui-state");
  await page.goto("/session/sess-a");
  await polled;
  await page.locator("body").click();
  await page.keyboard.press("ControlOrMeta+k");
  const search = page.getByPlaceholder("Search actions, sessions, settings…");
  await expect(search).toBeVisible();
  await search.fill(query);
}

test.describe("Plugin pages (#4460)", () => {
  test.beforeEach(async ({ page }) => {
    await page.setViewportSize({ width: 1280, height: 800 });
    await page.addInitScript(() => {
      localStorage.setItem("aoe_auth_token", "secret-token");
      const w = window as typeof window & { __frameReports: unknown[] };
      w.__frameReports = [];
      window.addEventListener("message", (e) => w.__frameReports.push(e.data));
    });
  });

  test("no Pages menu or palette group without a declared page", async ({ page }) => {
    await mockApis(page, []);
    await searchPalette(page, "board");
    await expect(page.getByRole("option", { name: /Board/ })).toHaveCount(0);
    await page.keyboard.press("Escape");
    await expect(page.getByRole("button", { name: "Pages" })).toHaveCount(0);
  });

  test("opens from the TopBar full width, sandboxed, and Back returns to the session", async ({ page }) => {
    await mockApis(page, [PAGE]);
    const order: string[] = [];
    page.on("request", (req) => {
      const path = new URL(req.url()).pathname;
      if (path.startsWith(PAGE.url)) order.push(`${req.method()} ${path}`);
    });
    const frameApiCalls: string[] = [];
    page.on("request", (req) => {
      if (req.frame() !== page.mainFrame() && req.url().includes("/api/sessions")) frameApiCalls.push(req.url());
    });

    await page.goto("/session/sess-a");
    await page.getByRole("button", { name: "Pages" }).click();
    await page.getByRole("menuitem", { name: "Board" }).click();

    await expect(page).toHaveURL("/plugins/acme.kit/board");
    await expect(page.getByRole("navigation", { name: "Sessions sidebar" })).toHaveCount(0);
    const frame = page.getByTestId("plugin-page-frame");
    await expect(frame).toHaveAttribute("sandbox", "allow-scripts");
    await expect(frame).toHaveAttribute("src", PAGE.url);
    await expect(page.frameLocator("[data-testid=plugin-page-frame]").getByText("board fixture")).toBeVisible();
    expect(order.slice(0, 2)).toEqual([`POST ${PAGE.url}/ticket`, `GET ${PAGE.url}`]);

    const report = await page.waitForFunction(
      () => (window as typeof window & { __frameReports: unknown[] }).__frameReports[0],
    );
    const { storage, cookie, api, href } = (await report.jsonValue()) as Record<string, string>;
    expect(storage).toMatch(/^blocked:/);
    expect(cookie).toMatch(/^blocked:/);
    expect(api).toMatch(/^blocked:/);
    expect(href).not.toContain("token");
    expect(frameApiCalls).toEqual([]);

    await page.getByRole("button", { name: "← Back" }).click();
    await expect(page).toHaveURL("/session/sess-a");
  });

  test("the palette lists the page; an unknown page shows an empty state", async ({ page }) => {
    await mockApis(page, [PAGE]);
    await searchPalette(page, "board");
    await page.getByRole("option", { name: /Board/ }).click();
    await expect(page).toHaveURL("/plugins/acme.kit/board");
    await expect(page.getByTestId("plugin-page-frame")).toBeVisible();

    await page.goto("/plugins/acme.kit/missing");
    await expect(page.getByText("Page not found")).toBeVisible();
    // A deep link has no in-app history, so Back lands on the dashboard.
    await page.getByRole("button", { name: "← Back" }).click();
    await expect(page).toHaveURL("/");
  });
});
