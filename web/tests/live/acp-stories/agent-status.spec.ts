// Agent status surfaces: rate-limit parking, plan progress, and startup failures.

import { join } from "node:path";
import type { Page } from "@playwright/test";
import { test, expect } from "../../helpers/liveTest";
import {
  chunk,
  endTurn,
  openStructuredView,
  postPrompt,
  replayFrames,
  script,
  sessionIdByTitle,
  startAcpSession,
  waitForReplayContains,
} from "../../helpers/acp";
import { seedSessionViaAoeAdd } from "../../helpers/aoeServe";

// The fake reports the reset only on a usage_update's meta, like claude-agent-acp.
const rateLimitedTurn = (resetsAt: number, extra: object = {}) => ({
  updates: [
    chunk("Starting the task."),
    {
      sessionUpdate: "usage_update",
      used: 1234,
      size: 200000,
      _meta: { "_claude/rateLimit": { status: "rejected", resetsAt, ...extra } },
    },
  ],
  rateLimit: { message: "usage limit reached" },
});

// Persist the fake's turn cursor so the resumed worker gets the next turn.
const persistTurnCursor = (home: string) => ({ FAKE_ACP_TURN_STATE: join(home, "fake-acp-turn-cursor") });

/** Distinct from the `now + 1h` fallback so a regression renders a different time. */
const resetIn = (hours: number, minutes: number) => Math.floor(Date.now() / 1000) + hours * 3600 + minutes * 60;

async function expectResetBanner(page: Page, resetSecs: number, timeout: number) {
  // Computed in-browser so locale and timezone match the UI.
  const expected = await page.evaluate((ms) => new Date(ms).toLocaleTimeString(), resetSecs * 1000);
  await expect(page.getByText(`resets at ${expected}`)).toBeVisible({ timeout });
}

test("resume re-issues the interrupted prompt so the agent continues", async ({ page, spawnServe }) => {
  // #3028: resume must respawn the worker and re-send the rate-limited prompt.
  const reset = resetIn(2, 37);
  const { serve, sessionId } = await startAcpSession(spawnServe, {
    title: "rl-resume",
    fakeAcpScript: script(rateLimitedTurn(reset), endTurn(chunk("Resumed and continued the task."))),
    extraEnv: persistTurnCursor,
  });
  await openStructuredView(page, serve, sessionId, "keep working on the task");
  await expect(page.getByText(/Rate-limited/i)).toBeVisible({ timeout: 15_000 });
  await expectResetBanner(page, reset, 15_000);

  await page.getByRole("button", { name: /Resume now/i }).click();
  await waitForReplayContains(serve.baseUrl, sessionId, "Resumed and continued the task.", { timeoutMs: 30_000 });
});

test("a later rejection still reports the reset captured on an earlier turn", async ({ page, spawnServe }) => {
  // #3152: the adapter sends no usage_update on a turn rejected outright, so turn 0's epoch must survive.
  const reset = resetIn(3, 41);
  const { serve, sessionId } = await startAcpSession(spawnServe, {
    title: "rl-across",
    fakeAcpScript: script(rateLimitedTurn(reset, { rateLimitType: "five_hour" }), {
      updates: [],
      rateLimit: { message: "usage limit reached" },
    }),
    extraEnv: persistTurnCursor,
  });
  await openStructuredView(page, serve, sessionId, "start the task");
  await expectResetBanner(page, reset, 15_000);

  await page.getByRole("button", { name: /Resume now/i }).click();
  const resets = async () =>
    (
      (await replayFrames(serve.baseUrl, sessionId)) as {
        event?: { RateLimit?: { info?: { resets_at?: string | null } } };
      }[]
    )
      .filter((f) => f?.event?.RateLimit !== undefined)
      .map((f) => {
        const iso = f.event?.RateLimit?.info?.resets_at;
        return typeof iso === "string" ? Math.floor(new Date(iso).getTime() / 1000) : null;
      });
  await expect.poll(resets, { timeout: 30_000, intervals: [200, 500, 1000] }).toEqual([reset, reset]);
  await expectResetBanner(page, reset, 30_000);
});

test("sidebar row shows a rate-limited indicator after a park", async ({ page, spawnServe }) => {
  // #1715, #3514: a parked session maps to Idle, so the row needs its own indicator.
  const { serve, sessionId } = await startAcpSession(spawnServe, {
    title: "sidebar-rl-a",
    fakeAcpScript: script(rateLimitedTurn(resetIn(1, 0))),
  });
  await openStructuredView(page, serve, sessionId, "kick off A");
  await expect(page.getByText(/Rate-limited/i)).toBeVisible({ timeout: 15_000 });

  await page.goto(serve.baseUrl);
  await expect(page.getByTitle(/Rate-limited/i)).toBeVisible({ timeout: 15_000 });
});

const planUpdate = (...entries: [string, string, string][]) => ({
  sessionUpdate: "plan",
  entries: entries.map(([content, status, priority]) => ({ content, status, priority })),
});

test("plan session update renders in PlanStrip", async ({ page, spawnServe }) => {
  const { serve, sessionId } = await startAcpSession(spawnServe, {
    title: "story-plan",
    fakeAcpScript: script(
      endTurn(
        planUpdate(
          ["Investigate the bug", "in_progress", "high"],
          ["Write a fix", "pending", "medium"],
          ["Add tests", "pending", "low"],
        ),
        chunk("Planned."),
      ),
    ),
  });
  await openStructuredView(page, serve, sessionId, "plan this work");
  // The expanded list and the sidebar row repeat these texts; the strip mounts first.
  await expect(page.getByText("Investigate the bug").first()).toBeVisible({ timeout: 15_000 });
  await expect(page.getByText("0/3").first()).toBeVisible({ timeout: 15_000 });
});

test("sidebar PlanProgressMini renders the structured view plan summary", async ({ page, spawnServe }) => {
  const { serve, sessionId } = await startAcpSession(spawnServe, {
    title: "story-sidebar-plan",
    fakeAcpScript: script(
      endTurn(
        planUpdate(["Step alpha", "in_progress", "high"], ["Step bravo", "pending", "medium"]),
        chunk("Planned."),
      ),
    ),
  });
  const res = await postPrompt(serve.baseUrl, sessionId, "plan it");
  if (!res.ok) throw new Error(`structured view prompt POST failed: ${res.status} ${await res.text()}`);

  await page.goto(serve.baseUrl);
  await expect(page.getByRole("progressbar", { name: /Plan progress: 0 of 2 steps/i })).toBeVisible({
    timeout: 20_000,
  });
});

test("startup banner: native-binary branch + agent-log disclosure", async ({ page, spawnServe }) => {
  // #1449: a session/new failure whose details match the native-binary regex.
  const serve = await spawnServe({
    acp: true,
    fakeAcpScript: {
      failOn: {
        method: "session/new",
        code: -32603,
        message: "Internal error",
        data: {
          details:
            "Claude Code native binary at /usr/lib/node_modules/@agentclientprotocol/claude-agent-acp/node_modules/@anthropic-ai/claude-agent-sdk-linux-arm64/claude exists but failed to launch.",
        },
      },
    },
    seedFn: seedSessionViaAoeAdd({ title: "story-native-binary" }),
  });
  const sessionId = await sessionIdByTitle(serve.baseUrl, "story-native-binary");
  // enableStructuredViewAndWait throws on the AgentStartupError this test wants.
  const enableRes = await fetch(`${serve.baseUrl}/api/sessions/${sessionId}/acp/enable`, { method: "POST" });
  expect(enableRes.ok).toBe(true);
  const startupError = async () =>
    ((await replayFrames(serve.baseUrl, sessionId)) as { event?: { AgentStartupError?: { message?: string } } }[])
      .map((f) => f.event?.AgentStartupError?.message)
      .find((m) => typeof m === "string" && m.length > 0) ?? "";
  await expect.poll(startupError, { timeout: 20_000, intervals: [200] }).not.toBe("");
  const msg = await startupError();
  expect(msg).toContain("native binary");
  expect(msg).toContain("failed to launch");

  await page.goto(`${serve.baseUrl}/session/${encodeURIComponent(sessionId)}`);
  await expect(page.getByText("Structured view agent failed to start")).toBeVisible({ timeout: 15_000 });
  await expect(page.getByText(/Architecture mismatch/i)).toBeVisible();
  await expect(page.getByText(/aoe acp doctor --fix/)).toHaveCount(0);

  const toggle = page.getByTestId("acp-agent-log-toggle");
  await expect(toggle).toBeVisible();
  await toggle.click();
  // Any terminal state of the disclosure proves the log endpoint round-tripped.
  const body = page.getByTestId("acp-agent-log-body");
  await expect(body).toBeVisible({ timeout: 10_000 });
  await expect(body).toHaveText(/Loading log|Could not load log|No log output yet|Log file exists but is empty|.+/);
  await page.getByTestId("acp-agent-log-refresh").click();
  await expect(body).toBeVisible({ timeout: 5_000 });
});
