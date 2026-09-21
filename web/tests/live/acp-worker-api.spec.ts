// Structured view worker endpoints: view switch, mode and config options, shutdown, files, worker log, custom agents.

import { mkdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { test, expect } from "../helpers/liveTest";
import { appDirFor, listSessions, resolveAoeBinary, seedSessionViaAoeAdd, waitForView } from "../helpers/aoeServe";
import {
  postAcp,
  replayJson,
  seedAcpSession,
  spawnAcpAgent,
  startAcpSession,
  waitForReplayContains,
} from "../helpers/acp";

const expect2xx = (res: Response) => {
  expect(res.status).toBeGreaterThanOrEqual(200);
  expect(res.status).toBeLessThan(300);
};

const isStructured = async (baseUrl: string, sessionId: string) =>
  (await listSessions(baseUrl)).find((s) => s.id === sessionId)?.view === "structured";

test("view switch round-trips between tmux and structured view", async ({ spawnServe }) => {
  const { serve, sessionId } = await seedAcpSession(spawnServe, { title: "acp-view" });
  // `aoe add` defaults to tmux.
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
  }
});

test("DELETE /acp shuts the worker down with 204 / 404", async ({ spawnServe }) => {
  const { serve, sessionId } = await seedAcpSession(spawnServe, { title: "acp-shutdown" });
  const shutdown = () => fetch(`${serve.baseUrl}/api/sessions/${sessionId}/acp`, { method: "DELETE" });
  // A boot-time reconcile may already have a pending spawn, so either status is valid before enable.
  expect([204, 404]).toContain((await shutdown()).status);

  // Enable spawns asynchronously; retry until the worker entry exists.
  await postAcp(serve.baseUrl, sessionId, "/enable");
  await expect.poll(async () => (await shutdown()).status, { timeout: 5_000, intervals: [200] }).toBe(204);

  // Unlike disable, shutdown keeps the structured view flag.
  await waitForView(serve.baseUrl, sessionId, "structured");
  // The reconciler may respawn a structured session, so a repeat can be either.
  expect([204, 404]).toContain((await shutdown()).status);
});

test("session/mode round-trips through the fake ACP agent", async ({ spawnServe }) => {
  const { serve, sessionId } = await seedAcpSession(spawnServe, { title: "mode-trace" });
  expect((await postAcp(serve.baseUrl, sessionId, "/enable")).ok).toBeTruthy();
  // Races enable's own spawn: 409 or 2xx both mean the supervisor registered the session.
  await spawnAcpAgent(serve.baseUrl, sessionId);
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
    const started = await seedAcpSession(spawnServe, { title, extraEnv });
    expect((await postAcp(started.serve.baseUrl, started.sessionId, "/enable")).ok).toBeTruthy();
    await spawnAcpAgent(started.serve.baseUrl, started.sessionId);
    return started;
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

  test("rejected set_config_option surfaces as ConfigOptionSwitchFailed", async ({ spawnServe }) => {
    const { serve, sessionId } = await start(spawnServe, "config-pickers-reject", {
      FAKE_ACP_REJECT_CONFIG_OPTION: "rate limited (test)",
    });
    // The request itself succeeds; the rejection arrives as an async event.
    expect2xx(
      await postAcp(serve.baseUrl, sessionId, "/config-option", { config_id: "model", value: "claude-sonnet-4-6" }),
    );
    await replayMatches(
      serve.baseUrl,
      sessionId,
      (json) => json.includes("ConfigOptionSwitchFailed") && json.includes("rate limited"),
    );
  });
});

test("structured view/files lists workspace files and honors the skip rules", async ({ spawnServe }) => {
  const serve = await spawnServe({
    acp: true,
    seedFn: (env) => {
      seedSessionViaAoeAdd({ title: "acp-files" })(env);
      const projectDir = join(env.home, "project");
      mkdirSync(join(projectDir, "src", "nested"), { recursive: true });
      mkdirSync(join(projectDir, "node_modules", "junk"), { recursive: true });
      writeFileSync(join(projectDir, "main.rs"), "fn main() {}\n");
      writeFileSync(join(projectDir, "src", "lib.rs"), "// lib\n");
      writeFileSync(join(projectDir, "src", "nested", "deep.rs"), "// deep\n");
      writeFileSync(join(projectDir, "node_modules", "junk", "ignore.js"), "// ignore\n");
      writeFileSync(join(projectDir, ".secret"), "should be hidden\n");
    },
  });
  const [session] = await listSessions(serve.baseUrl);
  expect(session).toBeDefined();

  const res = await fetch(`${serve.baseUrl}/api/sessions/${session!.id}/acp/files`);
  expect(res.ok).toBeTruthy();
  const body = (await res.json()) as { files: string[]; truncated: boolean };
  expect(Array.isArray(body.files)).toBe(true);
  expect(body.truncated).toBe(false);
  for (const file of ["main.rs", "src/lib.rs", "src/nested/deep.rs"]) expect(body.files).toContain(file);
  expect(body.files.some((f) => f.startsWith("node_modules/"))).toBe(false);
  expect(body.files).not.toContain(".secret");

  expect((await fetch(`${serve.baseUrl}/api/sessions/does-not-exist/acp/files`)).status).toBe(404);
});

// #1449: the runner stderr drain, readable without host terminal access.
test.describe("worker log", () => {
  type WorkerLog = { path: string; exists: boolean; tail: string; lines_returned: number; truncated: boolean };
  const workerLog = async (baseUrl: string, sessionId: string, tail: number) => {
    const res = await fetch(`${baseUrl}/api/sessions/${sessionId}/acp/worker-log?tail=${tail}`);
    expect(res.status).toBe(200);
    return (await res.json()) as WorkerLog;
  };

  test("worker-log is empty before the runner writes and 404s for an unknown session", async ({ spawnServe }) => {
    const { serve, sessionId } = await seedAcpSession(spawnServe, { title: "worker-log-empty" });
    const body = await workerLog(serve.baseUrl, sessionId, 200);
    expect(body.exists).toBe(false);
    expect(body.tail).toBe("");
    expect(body.lines_returned).toBe(0);
    expect(body.truncated).toBe(false);
    expect(body.path).toMatch(/acp-workers/);

    expect((await fetch(`${serve.baseUrl}/api/sessions/does-not-exist-9d34/acp/worker-log`)).status).toBe(404);
  });

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
        `[session.custom_agents]\n"oc-acp" = "true"\n"oc-terminal" = "true"\n\n[session.agent_acp_cmd]\n"oc-acp" = "true acp"\n`,
      );
    },
  });
  // Guards against the master atomic not seeding from config at boot.
  await fetch(`${serve.baseUrl}/api/acp/master`, {
    method: "PATCH",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ enabled: true }),
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
