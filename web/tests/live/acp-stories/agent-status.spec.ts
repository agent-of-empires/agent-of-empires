// Agent status surfaces: rate-limit parking, plan progress, and startup failures.

import { join } from "node:path";
import type { Page } from "@playwright/test";
import { test, expect } from "../../helpers/liveTest";
import {
  chunk,
  endTurn,
  openStructuredView,
  replayFrames,
  script,
  sessionIdByTitle,
  spawnAcpAgent,
  startAcpSession,
  waitForReplayContains,
} from "../../helpers/acp";
import { listSessions, seedSessionViaAoeAdd } from "../../helpers/aoeServe";
import type { SessionRecovery } from "../../../src/lib/acpTypes";

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
  const expected = await page.evaluate(
    (ms) => new Date(ms).toLocaleTimeString([], { hour: "numeric", minute: "2-digit" }),
    resetSecs * 1000,
  );
  await expect(page.getByTestId("usage-limit-reset")).toContainText(`Resets ${expected}`, { timeout });
}

test("resume re-issues the interrupted prompt so the agent continues", async ({ page, spawnServe }) => {
  // Resume delivers the interrupted prompt on the retained native conversation.
  const reset = resetIn(2, 37);
  const { serve, sessionId } = await startAcpSession(spawnServe, {
    title: "rl-resume",
    fakeAcpScript: script(rateLimitedTurn(reset), endTurn(chunk("Resumed and continued the task."))),
    extraEnv: persistTurnCursor,
  });
  await openStructuredView(page, serve, sessionId, "keep working on the task");
  await expect(page.getByTestId("usage-limit-banner")).toContainText("usage limit reached", { timeout: 15_000 });
  await expectResetBanner(page, reset, 15_000);

  await page.getByTestId("usage-limit-resume").click();
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

  await page.getByTestId("usage-limit-resume").click();
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
  await expect(page.getByTestId("usage-limit-banner")).toBeVisible({ timeout: 15_000 });

  await page.goto(serve.baseUrl);
  await expect(page.getByTitle(/Rate-limited/i)).toBeVisible({ timeout: 15_000 });
});

const planUpdate = (...entries: [string, string, string][]) => ({
  sessionUpdate: "plan",
  entries: entries.map(([content, status, priority]) => ({ content, status, priority })),
});

test("plan session update renders in PlanStrip and the sidebar PlanProgressMini", async ({ page, spawnServe }) => {
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
  // The sidebar row's PlanProgressMini summarizes the same plan.
  await expect(page.getByRole("progressbar", { name: /Plan progress: 0 of 3 steps/i })).toBeVisible({
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
    seedFn: seedSessionViaAoeAdd({ title: "story-native-binary", structuredView: true }),
  });
  const sessionId = await sessionIdByTitle(serve.baseUrl, "story-native-binary");
  // Spawn without waiting for readiness so the startup error remains observable.
  await spawnAcpAgent(serve.baseUrl, sessionId);
  const startupError = async () =>
    ((await replayFrames(serve.baseUrl, sessionId)) as { event?: { AgentStartupError?: { message?: string } } }[])
      .map((f) => f.event?.AgentStartupError?.message)
      .find((m) => typeof m === "string" && m.length > 0) ?? "";
  await expect.poll(startupError, { timeout: 20_000, intervals: [200] }).not.toBe("");

  await page.goto(`${serve.baseUrl}/session/${encodeURIComponent(sessionId)}`);
  // "Claude Code", not the message's "claude": the name comes from the daemon's install-info.
  await expect(page.getByRole("heading", { name: "Claude Code couldn't start in Structured view" })).toBeVisible({
    timeout: 15_000,
  });
  await expect(page.getByText(/Architecture mismatch/i)).toBeVisible();
  await expect(page.getByText(/aoe acp doctor --fix/)).toHaveCount(0);

  await page.getByTestId("startup-error-banner-details").click();
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

for (const incident of [
  { name: "provider", message: "Provider request failed" },
  { name: "auth", message: "Authentication required in the Agent" },
]) {
  test(`${incident.name} operation failure survives reopen and accepts only a new explicit operation on the same native history`, async ({
    page,
    spawnServe,
  }) => {
    const { serve, sessionId } = await startAcpSession(spawnServe, {
      title: `shared-recovery-${incident.name}`,
      fakeAcpScript: script(
        {
          updates: [chunk("Partial work before the rejected operation.")],
          error: { code: -32603, message: incident.message, data: { provider_body: "PRIVATE_PROVIDER_BODY_5055" } },
        },
        endTurn(chunk("The new explicit operation completed.")),
      ),
    });
    await expect
      .poll(async () => (await listSessions(serve.baseUrl)).find((row) => row.id === sessionId)?.acp_session_id, {
        timeout: 20_000,
      })
      .toEqual(expect.any(String));
    const before = (await listSessions(serve.baseUrl)).find((row) => row.id === sessionId)!;
    const mutations: Array<{ path: string; body: unknown }> = [];
    page.on("request", (request) => {
      const path = new URL(request.url()).pathname;
      if (request.method() === "POST" && path.startsWith(`/api/sessions/${sessionId}/`)) {
        mutations.push({ path, body: request.postDataJSON() });
      }
    });
    const rejected = `original blocked operation ${incident.name}`;
    await openStructuredView(page, serve, sessionId, rejected);
    let diagnostic = "";
    await expect
      .poll(
        async () => {
          const current = (await listSessions(serve.baseUrl)).find((row) => row.id === sessionId)!;
          const projection = current.recovery as SessionRecovery | null | undefined;
          diagnostic = projection?.diagnostic ?? "";
          return projection?.reason;
        },
        { timeout: 20_000 },
      )
      .toBe("operation_failed");
    const banner = page.getByTestId("session-recovery");
    await expect(banner).toHaveAttribute("data-session-id", sessionId);
    await expect(banner).toHaveAttribute("data-reason", "operation_failed");
    await expect(banner).toContainText(diagnostic);
    await expect(page.getByTestId("session-recovery-scope")).toContainText("Blocking scope: operation");
    await expect(page.getByTestId("session-recovery-runtime")).toContainText("does not prove the OS process is alive");
    // A generic RPC result contains no affirmative retryability evidence.
    await expect(page.getByTestId("session-recovery-retry-operation")).toBeDisabled();
    await expect(page.getByText("PRIVATE_PROVIDER_BODY_5055")).toHaveCount(0);

    await page.reload();
    await expect(banner).toContainText(diagnostic);
    await page.getByTestId("session-recovery-attach").click();
    const reopened = (await listSessions(serve.baseUrl)).find((row) => row.id === sessionId)!;
    expect(reopened.acp_session_id).toBe(before.acp_session_id);
    expect(reopened.id).toBe(before.id);
    expect(mutations.filter(({ path }) => /\/(?:restart|ensure|spawn)$/.test(path))).toEqual([]);
    expect(mutations.filter(({ path }) => path.endsWith("/acp/prompt")).map(({ body }) => body)).toEqual([
      expect.objectContaining({ text: rejected }),
    ]);

    const composer = page.getByRole("textbox", { name: /Send a message/i });
    const explicit = `new explicit operation after ${incident.name} intervention`;
    await composer.fill(explicit);
    await composer.press("Enter");
    await expect(page.getByText("The new explicit operation completed.")).toBeVisible({ timeout: 20_000 });
    await expect(banner).toHaveCount(0);
    const after = (await listSessions(serve.baseUrl)).find((row) => row.id === sessionId)!;
    expect(after.acp_session_id).toBe(before.acp_session_id);
    expect(mutations.filter(({ path }) => path.endsWith("/acp/prompt")).map(({ body }) => body)).toEqual([
      expect.objectContaining({ text: rejected }),
      expect.objectContaining({ text: explicit }),
    ]);
    expect(mutations.filter(({ path }) => /\/(?:restart|ensure|spawn)$/.test(path))).toEqual([]);
  });
}
