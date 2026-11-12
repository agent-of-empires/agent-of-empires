// Agent status surfaces: rate-limit parking, plan progress, and startup failures.

import { readFileSync } from "node:fs";
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
  startAcpSession,
  waitForReplayContains,
} from "../../helpers/acp";
import { appDirFor, resolveAoeBinary, seedSessionViaAoeAdd } from "../../helpers/aoeServe";

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
  await expect(body).not.toContainText("Loading log");
  const capture = test.info().outputPath("startup-native-binary-disclosure.png");
  await page.screenshot({ path: capture, fullPage: true });
  await test.info().attach("startup-native-binary-disclosure", { path: capture, contentType: "image/png" });
});

test("request refusal preserves the real Native owner, retained diagnostic, and explicit retry", async ({
  page,
  spawnServe,
}) => {
  const diagnostic = "Provider refused this request";
  const continued = "The next explicit request used the same native conversation.";
  const { serve, sessionId } = await startAcpSession(spawnServe, {
    title: "request-error-owner",
    fakeAcpScript: script(
      {
        updates: [chunk("Partial work survives the request failure.")],
        error: {
          code: -32603,
          message: diagnostic,
          data: { errorKind: "provider_error", privateTokenMarker: "must-not-leak" },
        },
      },
      endTurn(chunk(continued)),
    ),
  });
  const xdg = serve.env.XDG_CONFIG_HOME;
  expect(xdg).toBeTruthy();
  const registry = join(appDirFor(serve.home, xdg!, resolveAoeBinary()), "acp-workers", `${sessionId}.json`);
  const owner = () => {
    const raw = readFileSync(registry, "utf8");
    const record = JSON.parse(raw) as { pid: number; runner_identity: unknown };
    const generation = raw.match(/"generation"\s*:\s*(\d+)/)?.[1];
    expect(record.pid).toBeGreaterThan(1);
    expect(record.runner_identity).toEqual(expect.any(Object));
    expect(generation).toMatch(/^\d+$/);
    return { pid: record.pid, birth: record.runner_identity, generation };
  };
  const nativePrompts = () =>
    readFileSync(join(serve.home, "fake-acp.log"), "utf8")
      .split("\n")
      .filter((line) => line.includes("session/prompt prompt="));
  type Frame = { event?: Record<string, unknown> };
  const events = async () =>
    ((await replayFrames(serve.baseUrl, sessionId)) as Frame[]).map((frame) => frame.event ?? {});
  await waitForReplayContains(serve.baseUrl, sessionId, '"AcpSessionAssigned"');
  const originalOwner = owner();
  const assigned = (await events()).flatMap((event) =>
    event.AcpSessionAssigned === undefined ? [] : [event.AcpSessionAssigned],
  );
  expect(assigned).toHaveLength(1);
  const box = await openStructuredView(page, serve, sessionId, "First explicit request");
  await waitForReplayContains(serve.baseUrl, sessionId, diagnostic);
  const failed = await events();
  const failures = failed.flatMap((event) =>
    event.PromptRuntimeError === undefined
      ? []
      : [event.PromptRuntimeError as { message: string; evidence: Record<string, unknown> }],
  );
  expect(failures).toHaveLength(1);
  expect(failures[0].message).toBe(diagnostic);
  expect(failures[0].evidence).toMatchObject({
    source: "local_prompt_result",
    operation: "prompt",
    runtime: "unknown",
    blocking_scope: "operation",
    reported_code: -32603,
  });
  expect(failures[0].evidence).not.toHaveProperty("origin");
  expect(failures[0].evidence).not.toHaveProperty("retryable");
  expect(JSON.stringify(failed)).not.toContain("must-not-leak");
  expect(failed.some((event) => event.AgentStartupError !== undefined || event.SessionContextReset !== undefined)).toBe(
    false,
  );
  expect(nativePrompts()).toHaveLength(1);
  expect(owner()).toEqual(originalOwner);
  await expect(page.getByText("Action did not complete", { exact: true })).toBeVisible();
  await expect(page.getByText(diagnostic, { exact: true })).toBeVisible();
  await expect(page.getByText("Structured view worker stopped", { exact: true })).toHaveCount(0);
  await expect(page.getByRole("heading", { name: /couldn't start in Structured view/ })).toHaveCount(0);
  await expect(box).toBeEditable();
  await page.reload();
  await expect(page.getByText(diagnostic, { exact: true })).toBeVisible();
  await expect(box).toBeEditable();
  expect(owner()).toEqual(originalOwner);
  expect(nativePrompts()).toHaveLength(1);
  const capture = test.info().outputPath("request-refusal-native-owner-retained.png");
  await page.screenshot({ path: capture, fullPage: true, animations: "disabled" });
  await test.info().attach("request-refusal-native-owner-retained", { path: capture, contentType: "image/png" });
  await box.fill("Second explicit request");
  await box.press("Enter");
  await waitForReplayContains(serve.baseUrl, sessionId, continued);
  await expect(page.getByText(continued, { exact: true })).toBeVisible();
  await expect(page.getByText("Action did not complete", { exact: true })).toHaveCount(0);
  const after = await events();
  expect(owner()).toEqual(originalOwner);
  expect(after.flatMap((event) => (event.AcpSessionAssigned === undefined ? [] : [event.AcpSessionAssigned]))).toEqual(
    assigned,
  );
  expect(after.filter((event) => event.UserPromptSent !== undefined)).toHaveLength(2);
  expect(after.filter((event) => event.PromptRuntimeError !== undefined)).toHaveLength(1);
  expect(after.some((event) => event.AgentStartupError !== undefined || event.SessionContextReset !== undefined)).toBe(
    false,
  );
  expect(nativePrompts()).toHaveLength(2);
  await test.info().attach("request-refusal-native-owner-proof", {
    body: JSON.stringify(
      { originalOwner, currentOwner: owner(), assigned, failures, nativePrompts: nativePrompts() },
      null,
      2,
    ),
    contentType: "application/json",
  });
});
