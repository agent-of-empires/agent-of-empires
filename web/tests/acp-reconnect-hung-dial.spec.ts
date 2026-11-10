// A reconnect whose replay fetch never answers (mobile network after a resume) must still dial the socket.

import type { WebSocketRoute } from "@playwright/test";

import { test, expect } from "./helpers/mockedTest";
import { mockAcpSession, openStructuredSession, waitForComposerConnected } from "./helpers/acpMock";
import { emptyAcpState } from "../src/lib/acpTypes";
import { currentControlSnapshot } from "./helpers/acpControlSnapshot";

test("reconnect dials the socket even when the replay request hangs", async ({ page }) => {
  const mock = await mockAcpSession(page, { title: "story-hung-replay" });
  const routes: WebSocketRoute[] = [];
  // Registered after the mock's handler, so it wins and lets the test drop the socket.
  await page.routeWebSocket(/\/sessions\/[^/]+\/acp\/ws/, (route) => {
    routes.push(route);
    route.send(JSON.stringify(currentControlSnapshot(emptyAcpState(), { ready: true })));
  });

  await openStructuredSession(page, mock);
  await waitForComposerConnected(page);
  expect(routes).toHaveLength(1);

  await page.route(/\/acp\/replay(\?|$)/, () => {});
  await routes[0]!.close();

  await expect.poll(() => routes.length, { timeout: 20_000 }).toBe(2);
  await waitForComposerConnected(page);
});
