// Agent status surfaces: rate-limit parking, plan progress, and startup failures.

import { join } from "node:path";
import { existsSync, readFileSync, writeFileSync } from "node:fs";
import type { Page } from "@playwright/test";
import { test, expect, authHeaders, bootDashboard } from "../../helpers/liveTest";
import {
  chunk,
  endTurn,
  openStructuredView,
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

test("manual rate-limit spawn joins an automatic SDK resume and continues the original prompt", async ({
  page,
  spawnServe,
}, testInfo) => {
  test.setTimeout(120_000);
  const original = "keep working on the sdk original task";
  const probe = "verify the next sdk turn";
  const serve = await spawnServe({
    acp: true,
    sdkAcp: true,
    authMode: "passphrase",
    preloginViaHarness: true,
    extraEnv: (home) => ({
      SHIM_RATE_LIMIT_RESUME_STATE: join(home, "sdk-resume-state"),
      SHIM_RATE_LIMIT_RESUME_PROMPTS: join(home, "sdk-resume-prompts.jsonl"),
    }),
    seedFn: (seed) => {
      seed.env.AOE_LOG_LEVEL = "debug";
      seedSessionViaAoeAdd({ title: "rl-present-sdk" })(seed);
    },
  });
  const request = (path: string, init: RequestInit = {}) =>
    fetch(`${serve.baseUrl}${path}`, {
      ...init,
      headers: { ...authHeaders(serve), "Content-Type": "application/json", ...init.headers },
    });
  const listing = async () => {
    const response = await request("/api/sessions");
    expect(response.status).toBe(200);
    const body = await response.json();
    return (Array.isArray(body) ? body : body.sessions) as { id: string; title: string; acp_worker_state?: string }[];
  };
  const session = (await listing()).find((row) => row.title === "rl-present-sdk");
  expect(session).toBeDefined();
  const sessionId = session!.id;
  const settings = await request("/api/settings").then((response) => response.json());
  const updated = await request("/api/settings", {
    method: "PATCH",
    body: JSON.stringify({ acp: { ...settings.acp, rate_limit_auto_resume: true } }),
  });
  expect(updated.status).toBe(200);
  expect((await request("/api/settings").then((response) => response.json())).acp.rate_limit_auto_resume).toBe(true);
  const enabled = await request(`/api/sessions/${sessionId}/acp/enable`, { method: "POST" });
  expect(enabled.status).toBe(200);
  await expect
    .poll(async () => (await listing()).find((row) => row.id === sessionId)?.acp_worker_state, { timeout: 30_000 })
    .toBe("running");
  await bootDashboard(page, serve, `/session/${encodeURIComponent(sessionId)}`);
  const composer = page.getByRole("textbox", { name: /Send a message/i });
  await composer.fill(original);
  await composer.press("Enter");
  await expect(page.getByText(/Rate-limited/i)).toBeVisible({ timeout: 15_000 });
  const state = join(serve.home, "sdk-resume-state");
  const entered = `${state}.initialize.entered`;
  const prompts = join(serve.home, "sdk-resume-prompts.jsonl");
  const replay = async () => {
    const response = await request(`/api/sessions/${sessionId}/acp/replay?since=0`);
    expect(response.status).toBe(200);
    const body = (await response.json()) as {
      frames: {
        event?: { RateLimitAutoResumed?: { manual?: boolean }; AcpSessionAssigned?: unknown };
      }[];
    };
    return body.frames;
  };
  const canonical = () => {
    const rows = JSON.parse(readFileSync(join(serve.appDir, "profiles", "main", "sessions.json"), "utf8")) as {
      id: string;
      lifecycle_generation: number;
      runner_journal: {
        launches: {
          nonce: number[];
          boot: number[];
          generation: number;
          incarnation: unknown;
          profile_identity: unknown;
        }[];
      };
    }[];
    return rows.find((row) => row.id === sessionId)!;
  };
  const births = (row: ReturnType<typeof canonical>) =>
    row.runner_journal.launches.map(({ nonce, boot, generation, incarnation, profile_identity }) => ({
      nonce,
      boot,
      generation,
      incarnation,
      profile_identity,
    }));
  try {
    await expect.poll(() => existsSync(entered), { timeout: 75_000, intervals: [100, 200, 500] }).toBe(true);
    const held = JSON.parse(readFileSync(entered, "utf8")) as { pid: number; turnCursor: number };
    expect(held.turnCursor).toBe(1);
    const before = await replay();
    expect(before.filter((frame) => frame.event?.RateLimitAutoResumed?.manual === false)).toHaveLength(1);
    expect(before.filter((frame) => frame.event?.AcpSessionAssigned !== undefined)).toHaveLength(1);
    const admitted = canonical();
    expect(admitted.lifecycle_generation).toBeGreaterThan(0);
    expect(births(admitted)).toContainEqual(
      expect.objectContaining({
        generation: admitted.lifecycle_generation,
        incarnation: expect.any(Object),
        profile_identity: expect.any(Object),
      }),
    );
    const manual = request(`/api/sessions/${sessionId}/acp/spawn`, {
      method: "POST",
      body: "{}",
      signal: AbortSignal.timeout(20_000),
    }).then(
      (response) => ({ response }),
      (error: unknown) => ({ error }),
    );
    await expect
      .poll(
        () => {
          const path = join(serve.appDir, "debug.log");
          return (
            existsSync(path) &&
            readFileSync(path, "utf8")
              .split("\n")
              .some(
                (line) =>
                  line.includes("manual rate-limit spawn joined present resume after releasing instance lock") &&
                  line.includes(sessionId),
              )
          );
        },
        { timeout: 10_000, intervals: [100, 200, 500] },
      )
      .toBe(true);
    writeFileSync(`${state}.initialize.release`, "release");
    const result = await manual;
    if ("error" in result) throw result.error;
    expect(result.response.status).toBe(200);
    expect(await result.response.json()).toMatchObject({ session_id: sessionId, status: "running" });
    await expect
      .poll(async () => JSON.stringify(await replay()), { timeout: 30_000 })
      .toContain(`sdk resumed: ${original}`);
    expect((await replay()).filter((frame) => frame.event?.RateLimitAutoResumed?.manual === true)).toHaveLength(1);
    await composer.fill(probe);
    await composer.press("Enter");
    await expect
      .poll(async () => JSON.stringify(await replay()), { timeout: 15_000 })
      .toContain(`sdk resumed: ${probe}`);
    const records = readFileSync(prompts, "utf8")
      .trim()
      .split("\n")
      .map((line) => JSON.parse(line) as { pid: number; text: string });
    expect(records.map((record) => record.text)).toEqual([original, original, probe]);
    expect(records[0].pid).not.toBe(held.pid);
    expect(records[1].pid).toBe(held.pid);
    expect(records[2].pid).toBe(held.pid);
    const after = canonical();
    expect(after.lifecycle_generation).toBe(admitted.lifecycle_generation);
    expect(births(after)).toEqual(births(admitted));
    expect((await replay()).filter((frame) => frame.event?.AcpSessionAssigned !== undefined)).toHaveLength(2);
    await testInfo.attach("sdk-resume-prompts", { path: prompts, contentType: "application/jsonl" });
    await testInfo.attach("sdk-held-initialize", { path: entered, contentType: "application/json" });
  } finally {
    writeFileSync(`${state}.initialize.release`, "release");
  }
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
