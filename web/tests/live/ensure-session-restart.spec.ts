// Restart-on-attach e2e for the web dashboard.
//
// Proves POST /api/sessions/{id}/ensure restarts a dead session and
// leaves a live one alone. Uses the harness's `seedFn` hook to run
// `aoe add` BEFORE serve boots so the server picks up the session in
// its initial `state.instances` cache load (a post-spawn `aoe add`
// writes to disk but the server never reloads).

import { spawnSync } from "node:child_process";
import { mkdirSync, writeFileSync, rmSync } from "node:fs";
import { join } from "node:path";
import { test as base, expect } from "@playwright/test";
import { spawnAoeServe, listSessions, seedSessionViaAoeAdd } from "../helpers/aoeServe";

function tmuxHasSession(home: string, name: string): boolean {
  const res = spawnSync("tmux", ["has-session", "-t", name], {
    env: {
      ...process.env,
      HOME: home,
      TMUX_TMPDIR: join(home, "tmux"),
    },
  });
  return res.status === 0;
}

base.describe("ensure_session restart flow", () => {
  base("dead session is restarted by /ensure, live session stays alive", async ({}, testInfo) => {
    const title = "e2e-restart";
    const serve = await spawnAoeServe({
      authMode: "none",
      workerIndex: testInfo.workerIndex,
      parallelIndex: testInfo.parallelIndex,
      seedFn: seedSessionViaAoeAdd({ title }),
    });

    try {
      const sessions = await listSessions(serve.baseUrl);
      expect(sessions.length).toBeGreaterThan(0);
      const sessionId: string = sessions[0]!.id;
      const tmuxName = `${serve.tmuxPrefix}${title}_${sessionId.slice(0, 8)}`;

      // The server's status_poll_loop runs every 2s and reconciles the
      // on-disk session record (initial status: Idle) against tmux. There
      // is no tmux session for this seed, so it eventually flips to Error,
      // but a freshly booted server may not have polled yet.
      await expect
        .poll(async () => (await listSessions(serve.baseUrl))[0]?.status, {
          timeout: 10_000,
        })
        .toBe("Error");
      expect(tmuxHasSession(serve.home, tmuxName)).toBe(false);

      const r1 = await fetch(`${serve.baseUrl}/api/sessions/${sessionId}/ensure`, { method: "POST" });
      expect(r1.ok).toBeTruthy();
      expect((await r1.json()).status).toBe("restarted");
      expect(tmuxHasSession(serve.home, tmuxName)).toBe(true);

      const hookDir = `/tmp/aoe-hooks/${sessionId}`;
      mkdirSync(hookDir, { recursive: true });
      writeFileSync(join(hookDir, "status"), "idle");

      const r2 = await fetch(`${serve.baseUrl}/api/sessions/${sessionId}/ensure`, { method: "POST" });
      expect((await r2.json()).status).toBe("alive");

      const r3 = await fetch(`${serve.baseUrl}/api/sessions/${sessionId}/ensure`, { method: "POST" });
      expect((await r3.json()).status).toBe("alive");

      const kill = spawnSync("tmux", ["kill-session", "-t", tmuxName], {
        env: {
          ...process.env,
          HOME: serve.home,
          TMUX_TMPDIR: join(serve.home, "tmux"),
        },
      });
      expect(kill.status).toBe(0);

      const r4 = await fetch(`${serve.baseUrl}/api/sessions/${sessionId}/ensure`, { method: "POST" });
      expect((await r4.json()).status).toBe("restarted");

      try {
        rmSync(hookDir, { recursive: true, force: true });
      } catch {
        // best-effort
      }
    } finally {
      await serve.stop();
    }
  });

  base("frontend shows Starting placeholder then connects", async ({ page }, testInfo) => {
    const title = "e2e-restart";
    const serve = await spawnAoeServe({
      authMode: "none",
      workerIndex: testInfo.workerIndex,
      parallelIndex: testInfo.parallelIndex,
      seedFn: seedSessionViaAoeAdd({ title }),
    });

    try {
      const sessions = await listSessions(serve.baseUrl);
      expect(sessions.length).toBeGreaterThan(0);
      const sessionId: string = sessions[0]!.id;
      const tmuxName = `${serve.tmuxPrefix}${title}_${sessionId.slice(0, 8)}`;

      spawnSync("tmux", ["kill-session", "-t", tmuxName], {
        env: {
          ...process.env,
          HOME: serve.home,
          TMUX_TMPDIR: join(serve.home, "tmux"),
        },
      });

      // Delay /ensure so Playwright reliably observes the "pending"
      // placeholder. Without this the live backend can resolve the
      // restart before the assertion's first retry, and the placeholder
      // mounts + unmounts inside a single frame.
      await page.route("**/api/sessions/*/ensure", async (route) => {
        await new Promise((r) => setTimeout(r, 2000));
        await route.continue();
      });

      await page.goto(`${serve.baseUrl}/`);
      const sessionButton = page.getByRole("link").filter({ hasText: "e2e-restart" }).first();
      await expect(sessionButton).toBeVisible();
      await sessionButton.click();

      await expect(page.getByText("Starting session...")).toBeVisible();
      await expect(page.getByText("Starting session...")).toBeHidden({
        timeout: 15_000,
      });
    } finally {
      await serve.stop();
    }
  });
});
