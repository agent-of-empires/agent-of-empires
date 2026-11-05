// Structured view worker endpoints: view switch, mode and config options, shutdown, worker log, custom agents.
// File listing skip rules, log-tail windowing and config-option rejection are pinned by Rust unit and
// integration tests (list_files_sorts_skips_and_caps, read_log_tail_windows_and_partial_lines, acp_model_respawn).

import { mkdirSync, realpathSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { test, expect, type ServeHandle } from "../helpers/liveTest";
import { SYSTEMD_OFF, appDirFor, listSessions, resolveAoeBinary } from "../helpers/aoeServe";
import {
  postAcp,
  replayJson,
  seedAcpSession,
  waitForAcpReady,
  startAcpSession,
  waitForReplayContains,
} from "../helpers/acp";

const expect2xx = (res: Response) => {
  expect(res.status).toBeGreaterThanOrEqual(200);
  expect(res.status).toBeLessThan(300);
};

const isStructured = async (baseUrl: string, sessionId: string) =>
  (await listSessions(baseUrl)).find((s) => s.id === sessionId)?.view === "structured";

async function seedNativeTranscript(serve: ServeHandle, sessionId: string) {
  const session = (await listSessions(serve.baseUrl)).find((s) => s.id === sessionId);
  expect(typeof session?.acp_session_id).toBe("string");
  expect(session?.acp_session_id).not.toBe("");
  expect(typeof session?.project_path).toBe("string");
  const cwd = realpathSync(session!.project_path as string);
  const dir = join(serve.home, ".claude", "projects", cwd.replace(/[^a-zA-Z0-9-]/g, "-"));
  mkdirSync(dir, { recursive: true });
  const message = { role: "user", content: [{ type: "text", text: "view handoff" }] };
  writeFileSync(join(dir, `${session!.acp_session_id}.jsonl`), `${JSON.stringify({ type: "user", cwd, message })}\n`);
  return session!.acp_session_id as string;
}

test("view switch round-trips between tmux and structured view", async ({ spawnServe }) => {
  const { serve, sessionId } = await startAcpSession(spawnServe, { title: "acp-view" });
  const nativeSessionId = await seedNativeTranscript(serve, sessionId);
  expect((await postAcp(serve.baseUrl, sessionId, "/disable")).ok).toBeTruthy();
  expect(await isStructured(serve.baseUrl, sessionId)).toBeFalsy();

  const spawnBeforeEnable = await postAcp(serve.baseUrl, sessionId, "/spawn", {});
  expect(spawnBeforeEnable.status).toBe(409);
  expect(((await spawnBeforeEnable.json()) as { error?: string }).error).toBe("not_structured");

  // Each switch is checked twice (idempotence); the list poll allows only cache scheduling latency.
  for (const [path, structured] of [
    ["/enable", true],
    ["/disable", false],
  ] as const) {
    const res = await postAcp(serve.baseUrl, sessionId, path);
    expect(res.ok).toBeTruthy();
    const body = (await res.json()) as { session_id: string; view?: string };
    if (structured) expect(body.session_id).toBe(sessionId);
    expect(body.view === "structured").toBe(structured);
    await expect
      .poll(() => isStructured(serve.baseUrl, sessionId), { timeout: 10_000, intervals: [100, 200, 400] })
      .toBe(structured);

    const again = await postAcp(serve.baseUrl, sessionId, path);
    expect(again.ok).toBeTruthy();
    expect(((await again.json()) as { view?: string }).view === "structured").toBe(structured);
    if (structured) {
      await waitForAcpReady(serve.baseUrl, sessionId, 30_000, serve.home);
      expect((await listSessions(serve.baseUrl)).find((s) => s.id === sessionId)?.acp_session_id).toBe(nativeSessionId);
    }
  }
});

test("DELETE /acp refuses terminal view and durably stops a structured worker", async ({ spawnServe }) => {
  const { serve, sessionId } = await startAcpSession(spawnServe, { title: "acp-shutdown" });
  const nativeSessionId = await seedNativeTranscript(serve, sessionId);
  expect((await postAcp(serve.baseUrl, sessionId, "/disable")).ok).toBeTruthy();
  const shutdown = () => fetch(`${serve.baseUrl}/api/sessions/${sessionId}/acp`, { method: "DELETE" });
  expect((await shutdown()).status).toBe(409);
  expect(await isStructured(serve.baseUrl, sessionId)).toBeFalsy();

  expect((await postAcp(serve.baseUrl, sessionId, "/enable")).ok).toBeTruthy();
  await waitForAcpReady(serve.baseUrl, sessionId, 30_000, serve.home);
  expect((await listSessions(serve.baseUrl)).find((s) => s.id === sessionId)?.acp_session_id).toBe(nativeSessionId);
  expect((await shutdown()).status).toBe(204);

  const stoppedState = async () => {
    const session = (await listSessions(serve.baseUrl)).find((s) => s.id === sessionId);
    return { view: session?.view, status: session?.status, worker: session?.acp_worker_state };
  };
  const stopped = { view: "structured", status: "Stopped", worker: "absent" };
  await expect.poll(stoppedState).toEqual(stopped);

  // User stop survives daemon reconstruction; only an explicit start may resume it.
  await serve.restart();
  await expect.poll(stoppedState).toEqual(stopped);
  expect((await shutdown()).status).toBe(204);
  await expect.poll(stoppedState).toEqual(stopped);

  const start = await fetch(`${serve.baseUrl}/api/sessions/${sessionId}/start`, { method: "POST" });
  expect(start.ok).toBeTruthy();
  await waitForAcpReady(serve.baseUrl, sessionId, 30_000, serve.home);
  expect((await shutdown()).status).toBe(204);
  await expect.poll(stoppedState).toEqual(stopped);
});

test("session/mode round-trips through the fake ACP agent", async ({ spawnServe }) => {
  const { serve, sessionId } = await startAcpSession(spawnServe, { title: "mode-trace" });
  expect2xx(await postAcp(serve.baseUrl, sessionId, "/mode", { mode_id: "plan" }));
  await waitForReplayContains(serve.baseUrl, sessionId, ["current_mode_changed", "CurrentModeChanged"]);
});

// #1403: set_config_option makes the fake emit a confirming config_option_update.
test.describe("config options", () => {
  const replayMatches = async (baseUrl: string, sessionId: string, predicate: (json: string) => boolean) =>
    expect.poll(async () => predicate(await replayJson(baseUrl, sessionId)), { timeout: 15_000 }).toBe(true);

  async function start(
    spawnServe: Parameters<typeof seedAcpSession>[0],
    title: string,
    extraEnv?: Record<string, string>,
  ) {
    return startAcpSession(spawnServe, { title, extraEnv });
  }

  test("initial snapshot lands in replay and set_config_option round-trips for model and effort", async ({
    spawnServe,
  }) => {
    const { serve, sessionId } = await start(spawnServe, "config-pickers-set");
    // The initial snapshot proves session/new completed before setting options.
    await replayMatches(
      serve.baseUrl,
      sessionId,
      (json) =>
        json.includes("ConfigOptionsUpdated") && json.includes("claude-opus-4-7") && json.includes("thought_level"),
    );

    expect2xx(
      await postAcp(serve.baseUrl, sessionId, "/config-option", { config_id: "model", value: "claude-sonnet-4-6" }),
    );
    await replayMatches(
      serve.baseUrl,
      sessionId,
      (json) => json.includes("ConfigOptionsUpdated") && json.includes("claude-sonnet-4-6"),
    );

    expect2xx(await postAcp(serve.baseUrl, sessionId, "/config-option", { config_id: "effort", value: "high" }));
    await replayMatches(
      serve.baseUrl,
      sessionId,
      (json) => json.includes("ConfigOptionsUpdated") && /"effort"[^}]*"current_value"\s*:\s*"high"/.test(json),
    );
  });
});

// #1449: the runner stderr drain, readable without host terminal access.
test.describe("worker log", () => {
  type WorkerLog = { path: string; exists: boolean; tail: string; lines_returned: number; truncated: boolean };
  const workerLog = async (baseUrl: string, sessionId: string, tail: number) => {
    const res = await fetch(`${baseUrl}/api/sessions/${sessionId}/acp/worker-log?tail=${tail}`);
    expect(res.status).toBe(200);
    return (await res.json()) as WorkerLog;
  };

  test("worker-log returns the runner tail after spawn and clamps oversized requests", async ({ spawnServe }) => {
    const { serve, sessionId } = await startAcpSession(spawnServe, { title: "worker-log-populated" });
    // The runner flushes its init tracing lines shortly after the handshake.
    await expect
      .poll(async () => (await workerLog(serve.baseUrl, sessionId, 200)).lines_returned, {
        timeout: 10_000,
        intervals: [200],
      })
      .toBeGreaterThan(0);
    const body = await workerLog(serve.baseUrl, sessionId, 200);
    expect(body.exists).toBe(true);
    expect(body.tail.length).toBeGreaterThan(0);

    // Clamping is silent: no 4xx, bounded by the server max.
    expect((await workerLog(serve.baseUrl, sessionId, 999_999)).lines_returned).toBeLessThanOrEqual(2000);
  });
});

test("custom agent with agent_acp_cmd runs in structured view", async ({ spawnServe }) => {
  // #1579: only a custom agent with agent_acp_cmd is structured-capable; the server downgrades the other.
  const serve = await spawnServe({
    seedFn: ({ home, xdg }) => {
      const appDir = appDirFor(home, xdg, resolveAoeBinary());
      mkdirSync(appDir, { recursive: true });
      writeFileSync(
        join(appDir, "config.toml"),
        `[session.custom_agents]\n"oc-acp" = "true"\n"oc-terminal" = "true"\n\n[session.agent_acp_cmd]\n"oc-acp" = "true acp"\n\n${SYSTEMD_OFF}`,
      );
    },
  });

  const agentsRes = await fetch(`${serve.baseUrl}/api/agents`);
  expect(agentsRes.ok).toBeTruthy();
  const agents = (await agentsRes.json()) as Array<{ name: string; acp_capable: boolean }>;

  for (const [tool, capable] of [
    ["oc-acp", true],
    ["oc-terminal", false],
  ] as const) {
    const agent = agents.find((a) => a.name === tool);
    expect(agent, `${tool} missing from /api/agents`).toBeTruthy();
    expect(agent!.acp_capable).toBe(capable);

    const res = await fetch(`${serve.baseUrl}/api/sessions`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ path: "", tool, title: `${tool}-custom`, view: "structured", scratch: true }),
    });
    expect(res.ok, `POST /api/sessions failed: ${res.status} ${await res.clone().text()}`).toBeTruthy();
    const json = await res.json();
    const session = (json.session ?? json) as Record<string, unknown>;
    expect(session.view === "structured").toBe(capable);
    expect(session.acp_capable).toBe(capable);
  }
});
