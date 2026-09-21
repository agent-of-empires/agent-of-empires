// Session and server lifecycle against a real server: open and delete, restart on attach, reconnects, peer edits, mode switch.

import { spawnSync } from "node:child_process";
import { chmodSync, mkdirSync, rmSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { test, expect } from "../helpers/liveTest";
import { listSessions, resolveAoeBinary, seedSessionViaAoeAdd, waitForView } from "../helpers/aoeServe";

/** Seed a session whose fake claude prints `marker` so the live terminal is observable. */
const seedPrinting =
  (title: string, marker: string) => (seed: Parameters<ReturnType<typeof seedSessionViaAoeAdd>>[0]) => {
    seedSessionViaAoeAdd({ title })(seed);
    writeFileSync(join(seed.shimBin, "claude"), `#!/bin/sh\necho ${marker}\nexec tail -f /dev/null\n`, { mode: 0o755 });
  };

// Raw tmux calls must use the harness socket; debug builds ignore TMUX_TMPDIR (#2608).
const tmux = (socket: string, ...args: string[]) => spawnSync("tmux", ["-S", socket, ...args]);
const tmuxHasSession = (socket: string, name: string) => tmux(socket, "has-session", "-t", name).status === 0;

test("create, view, delete a session via live backend", async ({ page, spawnServe }) => {
  const serve = await spawnServe({ seedFn: seedPrinting("golden", "GOLDEN_LIVE_READY") });
  const [session] = await listSessions(serve.baseUrl);
  const sessionId = session!.id;

  await page.goto(`${serve.baseUrl}/`);
  const sessionRow = page.getByRole("link").filter({ hasText: "golden" }).first();
  await expect(sessionRow).toBeVisible({ timeout: 10_000 });
  await sessionRow.click();
  await expect(page).toHaveURL(new RegExp(`/session/${sessionId}`), { timeout: 10_000 });
  await page.locator("[data-live-terminal]").first().waitFor({ state: "visible", timeout: 10_000 });
  await expect(page.locator("[data-live-content]").filter({ hasText: "GOLDEN_LIVE_READY" })).toBeVisible();
  // The web attach hides tmux's status line.
  await expect(page.locator("body")).not.toContainText("to detach");

  const deleteRes = await fetch(`${serve.baseUrl}/api/sessions/${sessionId}`, { method: "DELETE" });
  expect(deleteRes.ok).toBeTruthy();
  await expect(sessionRow).toBeHidden({ timeout: 10_000 });
  expect((await listSessions(serve.baseUrl)).find((s) => s.id === sessionId)).toBeUndefined();
});

test.describe("ensure_session restart flow", () => {
  const title = "e2e-restart";

  test("dead session is restarted by /ensure, live session stays alive", async ({ spawnServe }) => {
    const serve = await spawnServe({ seedFn: seedSessionViaAoeAdd({ title }) });
    const [session] = await listSessions(serve.baseUrl);
    const sessionId = session!.id;
    const tmuxName = `${serve.tmuxPrefix}${title}_${sessionId.slice(0, 8)}`;
    const ensure = async () =>
      (await fetch(`${serve.baseUrl}/api/sessions/${sessionId}/ensure`, { method: "POST" }).then((r) => r.json()))
        .status;

    // The status poller flips the seeded session to Error once it sees no tmux session.
    await expect.poll(async () => (await listSessions(serve.baseUrl))[0]?.status, { timeout: 10_000 }).toBe("Error");
    expect(tmuxHasSession(serve.tmuxSocket, tmuxName)).toBe(false);
    expect(await ensure()).toBe("restarted");
    expect(tmuxHasSession(serve.tmuxSocket, tmuxName)).toBe(true);

    const hookBase = `/tmp/aoe-hooks-${process.getuid?.() ?? 0}`;
    const hookDir = `${hookBase}/${sessionId}`;
    mkdirSync(hookDir, { recursive: true });
    for (const dir of [hookBase, hookDir]) {
      try {
        chmodSync(dir, 0o700);
      } catch {
        // Not ours to chmod; the hook system would already be unusable.
      }
    }
    writeFileSync(join(hookDir, "status"), "idle");
    try {
      expect(await ensure()).toBe("alive");
      expect(await ensure()).toBe("alive");
      expect(tmux(serve.tmuxSocket, "kill-session", "-t", tmuxName).status).toBe(0);
      expect(await ensure()).toBe("restarted");
    } finally {
      rmSync(hookDir, { recursive: true, force: true });
    }
  });

  test("frontend shows Starting placeholder then connects", async ({ page, spawnServe }) => {
    const serve = await spawnServe({ seedFn: seedPrinting(title, "ENSURE_CONNECTED") });
    const [session] = await listSessions(serve.baseUrl);
    tmux(serve.tmuxSocket, "kill-session", "-t", `${serve.tmuxPrefix}${title}_${session!.id.slice(0, 8)}`);

    let releaseEnsure!: () => void;
    const ensureGate = new Promise<void>((resolve) => (releaseEnsure = resolve));
    await page.route("**/api/sessions/*/ensure", async (route) => {
      await ensureGate;
      await route.continue();
    });
    try {
      await page.goto(`${serve.baseUrl}/`);
      const sessionButton = page.getByRole("link").filter({ hasText: title }).first();
      await expect(sessionButton).toBeVisible();
      await sessionButton.click();
      // The agent and paired shell panes both render the placeholder; only the agent's /ensure is held.
      await expect(page.getByText("Starting session...").first()).toBeVisible();
      releaseEnsure();
      await expect(page.locator("[data-live-content]").filter({ hasText: "ENSURE_CONNECTED" })).toBeVisible();
      await expect(page.getByText("Starting session...").first()).toBeHidden({ timeout: 15_000 });
    } finally {
      releaseEnsure();
    }
  });
});

test("SIGTERM surfaces the alert; restart() clears it and flashes reconnected", async ({ serve, page }) => {
  // Filter by text: an unrelated dnd-kit live region is also role=status.
  const alertBanner = page.getByRole("alert").filter({ hasText: /server unreachable/i });
  const reconnectedBanner = page.getByRole("status").filter({ hasText: /reconnected/i });
  await page.goto(serve.baseUrl, { waitUntil: "domcontentloaded" });
  // The first successful sessions poll must land before the kill, or "Reconnected" fires early.
  await page.waitForResponse((r) => r.url().endsWith("/api/sessions") && r.status() === 200, { timeout: 10_000 });
  await expect(alertBanner).toBeHidden();
  await expect(reconnectedBanner).toBeHidden();

  // Kill separately from restart() so the 3s sessions poll can observe the outage.
  serve.proc.kill("SIGTERM");
  await expect(alertBanner).toBeVisible({ timeout: 8_000 });
  await serve.restart();
  await expect
    .poll(
      () =>
        fetch(`${serve.baseUrl}/api/about`).then(
          (r) => r.status,
          () => -1,
        ),
      { timeout: 10_000 },
    )
    .toBe(200);
  await expect(reconnectedBanner).toBeVisible({ timeout: 8_000 });
  // The flash dismisses itself after 3s.
  await expect(reconnectedBanner).toBeHidden({ timeout: 6_000 });
  await expect(alertBanner).toBeHidden();
});

test("peer rename surfaces within the watcher budget", async ({ page, spawnServe }) => {
  const serve = await spawnServe({ seedFn: seedSessionViaAoeAdd({ title: "peer-source" }) });
  await page.goto(`${serve.baseUrl}/`);
  await expect(page.getByText("peer-source")).toBeVisible({ timeout: 10_000 });

  const rename = spawnSync(resolveAoeBinary(), ["session", "rename", "peer-source", "-t", "peer-target"], {
    env: serve.env,
    stdio: "pipe",
  });
  expect(rename.status, rename.stderr.toString()).toBe(0);
  // The file watcher must beat the daemon's 2s status poll.
  await expect
    .poll(async () => (await listSessions(serve.baseUrl)).some((s) => s.title === "peer-target"), { timeout: 1_500 })
    .toBe(true);
  // The UI follows on its own 3s poll, so allow several cycles.
  await expect(page.getByText("peer-target")).toBeVisible({ timeout: 10_000 });
});

test("enabling structured view reaps agent, terminal, and tool tmux sessions", async ({ spawnServe }) => {
  // #1869: history is destroyed in the swap, so a surviving pane would orphan an unreachable shell.
  const title = "modeswitch";
  const serve = await spawnServe({ acp: true, seedFn: seedSessionViaAoeAdd({ title, tool: "claude" }) });
  const [session] = await listSessions(serve.baseUrl);
  const sessionId = session!.id;
  expect(session!.view === "structured").toBeFalsy();

  // Reapers scan list-sessions by these naming conventions.
  const id8 = sessionId.slice(0, 8);
  const names = [
    `${serve.tmuxPrefix}${title}_${id8}`,
    `${serve.tmuxPrefix}term_${title}_${id8}`,
    `${serve.tmuxPrefix}tool_lazygit_${title}_${id8}`,
  ];
  try {
    for (const name of names) {
      expect(
        tmux(serve.tmuxSocket, "new-session", "-d", "-s", name, "-x", "80", "-y", "24", "sleep", "600").status,
      ).toBe(0);
      expect(tmuxHasSession(serve.tmuxSocket, name)).toBe(true);
    }

    const enableRes = await fetch(`${serve.baseUrl}/api/sessions/${sessionId}/acp/enable`, { method: "POST" });
    expect(enableRes.ok).toBeTruthy();
    const enableBody = (await enableRes.json()) as { session_id: string; view?: string };
    expect(enableBody.session_id).toBe(sessionId);
    expect(enableBody.view === "structured").toBe(true);

    // SIGTERM grace and tmux settling can outlast the response.
    for (const name of names) {
      await expect
        .poll(() => tmuxHasSession(serve.tmuxSocket, name), { timeout: 10_000, intervals: [100, 200, 400] })
        .toBe(false);
    }
    await waitForView(serve.baseUrl, sessionId, "structured");
    // Killing tmux removes the poller that would settle a stale terminal status, so enable resets it.
    await expect
      .poll(async () => (await listSessions(serve.baseUrl)).find((s) => s.id === sessionId)?.status, {
        timeout: 10_000,
        intervals: [100, 200, 400],
      })
      .toBe("Idle");
  } finally {
    for (const name of names) tmux(serve.tmuxSocket, "kill-session", "-t", name);
  }
});
