// The sidebar's queued chip says what the Daemon's queue waits on, and follows a queue changed in another tab.

import { test, expect } from "./helpers/mockedTest";
import { mockAcpSession, openStructuredSession } from "./helpers/acpMock";
import { installSidebarMocks } from "./helpers/sidebarMocks";
import { sessionResponse } from "./helpers/sessions";
import type { QueueReasonWire } from "../src/lib/types";

test("the queued chip follows the polled reason without a reload", async ({ page }) => {
  let reason: QueueReasonWire = "turn_active";
  const row = { id: "q", title: "queued", project_path: "/tmp/repo", branch: null };
  await installSidebarMocks(page, { sessions: [row] });
  await page.route("**/api/agents", (r) =>
    r.fulfill({
      json: [
        {
          name: "codex",
          display_name: "Codex",
          kind: "builtin",
          binary: "codex",
          host_only: false,
          installed: true,
          install_hint: "",
        },
      ],
    }),
  );
  await page.route(/\/api\/sessions(\?.*)?$/, (r) =>
    r.fulfill({
      headers: { "aoe-runtime-epoch": "mock-runtime", "aoe-runtime-revision": "1" },
      json: {
        sessions: [
          sessionResponse({
            ...row,
            tool: "codex",
            view: "structured",
            acp_agent: "codex",
            queued_count: 1,
            queue_reason: reason,
          }),
        ],
        workspace_ordering: ["/tmp/repo::__session__::q"],
      },
    }),
  );
  await page.goto("/");

  await expect(page.getByTitle("1 queued message · sends when Codex finishes")).toBeVisible();
  reason = "compacting";
  await expect(page.getByTitle("1 queued message · sends after compacting")).toBeVisible({ timeout: 10_000 });
});

test("a message another tab queues reaches the open session's chip and strip on the next poll", async ({ page }) => {
  const at = new Date().toISOString();
  const queue = [{ id: "q1", seq: 1, text: "first", created_at: at }];
  // Read on every session-list request, so a change here is what the next poll reports.
  const polled = { queued_count: 1 };
  const mock = await mockAcpSession(page, { title: "story-other-tab", sessionFields: polled });
  await page.route("**/api/sessions/*/queue", (r) => r.fulfill({ json: queue }));
  await openStructuredSession(page, mock);
  await expect(page.getByTestId("queued-strip-header")).toContainText("Queued (1)");
  await expect(page.getByTitle(/^1 queued message\b/)).toBeVisible();

  queue.push({ id: "q2", seq: 2, text: "from another tab", created_at: at });
  polled.queued_count = 2;
  await expect(page.getByTestId("queued-strip-header")).toContainText("Queued (2)", { timeout: 10_000 });
  await expect(page.getByTitle(/^2 queued messages\b/)).toBeVisible();
});
