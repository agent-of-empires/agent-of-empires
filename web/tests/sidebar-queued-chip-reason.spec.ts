// The sidebar's queued chip says what the Daemon's queue waits on, from the session list alone.

import { test, expect } from "./helpers/mockedTest";
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
