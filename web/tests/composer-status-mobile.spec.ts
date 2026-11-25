// #5946: a long composer-status entry must wrap and truncate inside the
// footer at a phone width instead of overflowing it or pushing Send off screen.
// Real CSS, which jsdom cannot reproduce.

import { test, expect } from "./helpers/mockedTest";
import { mockAcpSession, openStructuredSession, usageUpdated, waitForComposerConnected } from "./helpers/acpMock";

const LONG_STATUS = "a-deliberately-very-long-composer-status-text-that-would-overflow-a-narrow-footer";

test("long composer-status stays inside the footer at 360px", async ({ page }) => {
  await page.setViewportSize({ width: 360, height: 740 });
  const mock = await mockAcpSession(page, {
    title: "composer-status-mobile",
    initialEvents: [usageUpdated({ used: 120_000, size: 200_000, cost: null })],
  });
  await page.route("**/api/plugins/ui-state", (r) =>
    r.fulfill({
      json: {
        entries: [
          {
            plugin_id: "acme.kit",
            slot: "composer-status",
            id: "status",
            session_id: mock.sessionId,
            payload: { text: LONG_STATUS },
          },
        ],
        notifications: [],
      },
    }),
  );
  await openStructuredSession(page, mock);
  await waitForComposerConnected(page);

  const status = page.getByTestId("plugin-composer-status");
  await expect(status).toBeVisible();
  await expect(page.getByTestId("composer-usage")).toBeVisible();

  const footer = page.getByTestId("composer-footer");
  const footerOverflows = await footer.evaluate((el) => el.scrollWidth > el.clientWidth);
  expect(footerOverflows).toBe(false);

  const text = status.locator("[data-plugin-slot] > span.truncate");
  const clipped = await text.evaluate((el) => ({
    ellipsis: getComputedStyle(el).textOverflow === "ellipsis",
    truncated: el.scrollWidth > el.clientWidth,
  }));
  expect(clipped).toEqual({ ellipsis: true, truncated: true });
  expect(await status.evaluate((el) => el.scrollWidth > el.clientWidth)).toBe(false);

  const viewport = page.viewportSize()!;
  const send = await page.getByRole("button", { name: "Send message" }).boundingBox();
  expect(send!.x + send!.width).toBeLessThanOrEqual(viewport.width);
  const box = await status.boundingBox();
  expect(box!.x + box!.width).toBeLessThanOrEqual(viewport.width);
});

test("many composer-status items stay capped and keep Send on screen", async ({ page }) => {
  await page.setViewportSize({ width: 360, height: 480 });
  const mock = await mockAcpSession(page, {
    title: "composer-status-many",
    initialEvents: [usageUpdated({ used: 120_000, size: 200_000, cost: null })],
  });
  await page.route("**/api/plugins/ui-state", (r) =>
    r.fulfill({
      json: {
        entries: [
          {
            plugin_id: "acme.kit",
            slot: "composer-status",
            id: "status",
            session_id: mock.sessionId,
            payload: { items: Array.from({ length: 12 }, (_, i) => ({ text: `status item ${i + 1}` })) },
          },
        ],
        notifications: [],
      },
    }),
  );
  await openStructuredSession(page, mock);
  await waitForComposerConnected(page);

  const status = page.getByTestId("plugin-composer-status");
  await expect(status).toBeVisible();
  const { height, scrollHeight, clientHeight } = await status.evaluate((el) => ({
    height: el.getBoundingClientRect().height,
    scrollHeight: el.scrollHeight,
    clientHeight: el.clientHeight,
  }));
  expect(height).toBeLessThanOrEqual(60);
  expect(scrollHeight).toBeGreaterThan(clientHeight);

  const viewport = page.viewportSize()!;
  const footer = await page.getByTestId("composer-footer").boundingBox();
  expect(footer!.y + footer!.height).toBeLessThanOrEqual(viewport.height);
  const actions = await page.getByTestId("composer-actions").boundingBox();
  expect(actions!.x + actions!.width).toBeLessThanOrEqual(viewport.width);
  expect(actions!.y + actions!.height).toBeLessThanOrEqual(viewport.height);
});
