// Live coverage for cross-process file-watch propagation in the dashboard:
//   - Spawn `aoe serve` against an isolated HOME.
//   - Seed one session via `aoe session add` (peer subprocess).
//   - Open the dashboard; assert the seeded session is visible.
//   - Issue a peer `aoe session rename` to mutate the on-disk
//     `sessions.json` from a different process.
//   - Assert the dashboard reflects the new title within the watcher
//     propagation budget (1.5s typical, 3s ceiling).
//
// Verifies the server-consumer migration (server-migration doc §8.2):
// `Storage::update` from a peer process triggers the kernel watcher
// in the daemon, fans into `disk_changed`, and the consumer task
// reloads `state.instances` so the dashboard sees the change without
// waiting for the 2s `status_poll_loop` tick.

import { test, expect } from "@playwright/test";
import { spawnSync } from "node:child_process";
import { spawnAoeServe, listSessions, seedSessionViaAoeAdd, resolveAoeBinary } from "../helpers/aoeServe";

const aoeBinary = resolveAoeBinary();

test.describe.serial("file-watch peer propagation", () => {
  test("peer rename surfaces within the watcher budget", async ({ page }, ti) => {
    const serve = await spawnAoeServe({
      authMode: "none",
      workerIndex: ti.workerIndex,
      parallelIndex: ti.parallelIndex,
      seedFn: seedSessionViaAoeAdd({ title: "peer-source" }),
    });
    try {
      await page.goto(`${serve.baseUrl}/`);
      await expect(page.getByText("peer-source")).toBeVisible({
        timeout: 10_000,
      });

      const rename = spawnSync(aoeBinary, ["session", "rename", "peer-source", "-t", "peer-target"], {
        env: serve.env,
        stdio: "pipe",
      });
      expect(rename.status, rename.stderr.toString()).toBe(0);

      // Prove the daemon state flips through the watcher path before the 2s
      // poll fallback could refresh it.
      await expect
        .poll(async () => (await listSessions(serve.baseUrl)).some((session) => session.title === "peer-target"), {
          timeout: 1_500,
        })
        .toBe(true);

      // Once the watcher has updated daemon state, the dashboard still has to
      // pick the change up on its own client poll (useSessions POLL_INTERVAL,
      // 3s) and repaint. The watcher-beats-poll guarantee is already proven by
      // the 1.5s daemon-state assertion above; this only confirms the UI
      // eventually reflects it, so the budget must comfortably exceed one poll
      // cycle. A 3s ceiling equalled the poll interval and raced (a rename
      // landing right after a poll missed the window). Allow several cycles.
      await expect(page.getByText("peer-target")).toBeVisible({
        timeout: 10_000,
      });
    } finally {
      await serve.stop();
    }
  });
});
