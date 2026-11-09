// Agent status surfaces: rate-limit parking, plan progress, and startup failures.

import { appendFileSync, existsSync, readFileSync, readdirSync, realpathSync, writeFileSync } from "node:fs";
import { DatabaseSync } from "node:sqlite";
import { join } from "node:path";
import type { Page } from "@playwright/test";
import { test, expect } from "../../helpers/liveTest";
import {
  chunk,
  endTurn,
  composer,
  enableStructuredViewAndWait,
  openStructuredView,
  replayFrames,
  script,
  sessionIdByTitle,
  seedTranscript,
  startAcpSession,
  waitForReplayContains,
} from "../../helpers/acp";
import { appDirFor, fakeAcpScriptPath, listSessions, seedSessionViaAoeAdd } from "../../helpers/aoeServe";

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

test("account swap refuses a failed durable quota breadcrumb without replaying Native input", async ({
  page,
  spawnServe,
}, testInfo) => {
  const title = "quota-publication-io";
  const original = "ORIGINAL_EXPLICIT_QUOTA_TASK";
  const followup = "NEXT_EXPLICIT_AFTER_QUOTA_IO_REFUSAL";
  const completed = "Native handled its next actual prompt.";
  const marker = "QUOTA_BREADCRUMB_SQLITE_IO_REFUSED";
  const serve = await spawnServe({
    acp: true,
    fakeAcpScript: { ...script(rateLimitedTurn(resetIn(2, 37)), endTurn(chunk(completed))), holdLoad: true },
    extraEnv: persistTurnCursor,
    seedFn: (seed) => {
      const app = appDirFor(seed.home, seed.env.XDG_CONFIG_HOME!, process.env.AOE_E2E_BINARY!);
      appendFileSync(
        join(app, "config.toml"),
        '\n[session]\nrestart_wake_message = "FORBIDDEN_DEFAULT_QUOTA_WAKE"\n' +
          '[session.agent_detect_as]\nquota-other-claude = "claude"\n' +
          '[session.agent_execution_as]\nquota-other-claude = "claude"\n' +
          '[session.agent_config_dir]\nclaude = "~/.claude"\nquota-other-claude = "~/.claude"\n',
      );
      seedSessionViaAoeAdd({ title })(seed);
    },
  });
  const sessionId = await sessionIdByTitle(serve.baseUrl, title);
  await enableStructuredViewAndWait(serve.baseUrl, sessionId, 30_000, serve.home);
  await openStructuredView(page, serve, sessionId, original);
  await expect(page.getByTestId("usage-limit-banner")).toContainText("usage limit reached", { timeout: 15_000 });
  const row = async () => (await listSessions(serve.baseUrl)).find((entry) => entry.id === sessionId)!;
  const before = await row();
  expect(typeof before.acp_session_id).toBe("string");
  const app = appDirFor(serve.home, serve.env.XDG_CONFIG_HOME!, process.env.AOE_E2E_BINARY!);
  const stored = () => {
    const profiles = join(app, "profiles");
    const records = readdirSync(profiles).flatMap((profile) => {
      const path = join(profiles, profile, "sessions.json");
      if (!existsSync(path)) return [];
      const data: unknown = JSON.parse(readFileSync(path, "utf8"));
      expect(Array.isArray(data), path).toBe(true);
      return data as Array<{ id: string; pending_initial_turn?: { text: string } }>;
    });
    const record = records.find((entry) => entry.id === sessionId);
    expect(record).toBeDefined();
    return record!;
  };
  expect(before.context_resume).toMatchObject({ state: "available" });
  await expect.poll(stored).toMatchObject({
    acp_session_id: before.acp_session_id,
    agent_session_binding: {
      session_id: before.acp_session_id,
      provenance: "observed",
      execution: {
        agent: "claude",
        filesystem: "host",
        cwd: before.project_path,
        stores: [join(serve.home, ".claude")],
      },
    },
  });
  seedTranscript(
    serve.home,
    realpathSync(before.project_path).replace(/[^a-zA-Z0-9-]/g, "-"),
    before.acp_session_id!,
    before.project_path,
    original,
  );
  const nativeLog = () => readFileSync(join(serve.home, "fake-acp.log"), "utf8");
  const inputs = () =>
    nativeLog()
      .split("\n")
      .filter((line) => line.includes("session/prompt prompt="))
      .map((line) => JSON.parse(line.split("session/prompt prompt=")[1]!));
  expect(inputs()).toEqual([[expect.objectContaining({ type: "text", text: original })]]);
  const dbPath = join(app, "acp_events.db");
  expect(existsSync(dbPath)).toBe(true);
  const db = new DatabaseSync(dbPath);
  const originalRows = db
    .prepare("SELECT seq,event_json FROM acp_events WHERE session_id=? ORDER BY seq")
    .all(sessionId);
  expect(stored().pending_initial_turn).toBeUndefined();
  db.exec(
    `CREATE TRIGGER quota_breadcrumb_io_failure BEFORE INSERT ON acp_events
     WHEN NEW.discriminant='RateLimitAutoResumed'
     BEGIN SELECT RAISE(ABORT,'${marker}'); END;`,
  );
  let restarting: Promise<{ status: number; body: string }> | undefined;
  try {
    let early: { status: number; body: string } | undefined;
    restarting = fetch(`${serve.baseUrl}/api/sessions/${sessionId}/restart`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ tool: "quota-other-claude", require_account_carry: true }),
    }).then(async (response) => {
      const result = { status: response.status, body: await response.text() };
      early = result;
      return result;
    });
    const heldLoad = () => nativeLog().match(/session\/load held sessionId=(\S+) pid=(\d+)/);
    await expect
      .poll(() => {
        const held = heldLoad();
        if (!held && early)
          throw new Error(`Restart finished before the actual Native load barrier: ${JSON.stringify(early)}`);
        return held?.[1] ?? null;
      })
      .toBe(before.acp_session_id);
    const peer = Number(heldLoad()![2]);
    process.kill(peer, 0);
    writeFileSync(`${fakeAcpScriptPath(serve.home)}.load-release`, "release actual Native load");
    const result = await restarting;
    await expect.poll(() => readFileSync(join(app, "debug.log"), "utf8")).toContain(marker);
    if (result.status < 400) {
      await waitForReplayContains(serve.baseUrl, sessionId, completed, { timeoutMs: 30_000 });
    }
    const atReturn = await row();
    process.kill(peer, 0);
    const facts = {
      result,
      nativePidCapturedBeforePublication: peer,
      nativeSidBefore: before.acp_session_id,
      nativeSidAfter: atReturn.acp_session_id,
      workerAfter: atReturn.acp_worker_state,
      pendingAfter: stored().pending_initial_turn ?? null,
      actualInputs: inputs(),
      sqliteFailureObserved: true,
    };
    await testInfo.attach("actual-quota-publication-refusal", {
      body: JSON.stringify(facts),
      contentType: "application/json",
    });
    console.log("ACTUAL_QUOTA_PUBLICATION_IO", JSON.stringify(facts));
    expect(result.status, result.body).toBeGreaterThanOrEqual(400);
    expect(atReturn.acp_worker_state).toBe("running");
    expect(atReturn.acp_session_id).toBe(before.acp_session_id);
    expect(stored().pending_initial_turn).toBeUndefined();
    expect(inputs()).toEqual([[expect.objectContaining({ type: "text", text: original })]]);
    expect(db.prepare("SELECT seq,event_json FROM acp_events WHERE session_id=? ORDER BY seq").all(sessionId)).toEqual(
      expect.arrayContaining(originalRows),
    );
    await expect(composer(page)).toBeEnabled();
    await composer(page).fill(followup);
    await composer(page).press("Enter");
    await waitForReplayContains(serve.baseUrl, sessionId, completed, { timeoutMs: 30_000 });
    process.kill(peer, 0);
    expect((await row()).acp_session_id).toBe(before.acp_session_id);
    expect(inputs()).toEqual([
      [expect.objectContaining({ type: "text", text: original })],
      [expect.objectContaining({ type: "text", text: followup })],
    ]);
    const capture = testInfo.outputPath("quota-publication-refused-retained-native.png");
    await page.screenshot({ path: capture, fullPage: true });
    await testInfo.attach("quota-publication-refused-retained-native", { path: capture, contentType: "image/png" });
  } finally {
    writeFileSync(`${fakeAcpScriptPath(serve.home)}.load-release`, "release actual Native load during cleanup");
    if (restarting) await Promise.allSettled([restarting]);
    db.exec("DROP TRIGGER quota_breadcrumb_io_failure");
    db.close();
  }
});
