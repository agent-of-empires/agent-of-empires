// Lazy plugin panes: the worker pushes only a stub, and the dashboard fetches the content when the pane is opened.

import { test, expect } from "./helpers/mockedTest";
import type { Page } from "@playwright/test";
import { mockTerminalApis } from "./helpers/terminal-mocks";
import { openPanelsByDefault } from "./helpers/apiMocks";

const SESSION = "pinch-test";
const PANE_TOGGLE = '[data-testid="pane-toggle-plugin:acme.lazy:lazy_pane"]';
const PANE_ROUTE = /\/api\/plugins\/acme\.lazy\/panes\/lazy_pane\?/;

async function mockLazyStub(page: Page) {
  await mockTerminalApis(page);
  await openPanelsByDefault(page);
  await page.setViewportSize({ width: 1280, height: 720 });
  await page.route("**/api/plugins/ui-state", (route) =>
    route.fulfill({
      json: {
        entries: [
          {
            plugin_id: "acme.lazy",
            slot: "pane",
            id: "lazy_pane",
            session_id: SESSION,
            lazy: true,
            payload: { title: "Lazy", default_location: "right" },
          },
        ],
        notifications: [],
      },
    }),
  );
}

test.describe("Lazy plugin panes", () => {
  test("opening a lazy pane fetches its content for the session and renders it", async ({ page }) => {
    await mockLazyStub(page);
    const fetched: string[] = [];
    await page.route(PANE_ROUTE, (route) => {
      fetched.push(new URL(route.request().url()).searchParams.get("session_id") ?? "");
      return route.fulfill({ json: { blocks: [{ kind: "heading", text: "Rendered on demand" }] } });
    });

    await page.goto(`/session/${SESSION}`);
    await page.getByTestId("pane-menu-toggle").click();
    await expect(page.locator(PANE_TOGGLE)).toBeVisible();
    expect(fetched).toEqual([]);

    await page.locator(PANE_TOGGLE).click();
    await expect(page.getByText("Rendered on demand")).toBeVisible();
    expect(fetched).toEqual([SESSION]);
  });

  test("a pane the host cannot render shows a failed state instead of the stub", async ({ page }) => {
    await mockLazyStub(page);
    await page.route(PANE_ROUTE, (route) =>
      route.fulfill({ status: 502, json: { error: "pane_failed", message: "Plugin acme.lazy could not render" } }),
    );

    await page.goto(`/session/${SESSION}`);
    await page.getByTestId("pane-menu-toggle").click();
    await page.locator(PANE_TOGGLE).click();
    await expect(page.getByTestId("plugin-pane-failed")).toHaveText("Could not load this pane.");
  });
});
