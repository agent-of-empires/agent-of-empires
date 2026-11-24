// Structured view worker endpoints: view switch, mode and config options, shutdown, worker log, custom agents.
// File listing skip rules, log-tail windowing and config-option rejection are pinned by Rust unit and
// integration tests (list_files_sorts_skips_and_caps, read_log_tail_windows_and_partial_lines, acp_model_respawn).

import { mkdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { test, expect } from "../helpers/liveTest";
import { SYSTEMD_OFF, appDirFor, listSessions, resolveAoeBinary } from "../helpers/aoeServe";
import {
  openSession,
  postAcp,
  replayJson,
  seedAcpSession,
  waitForAcpReady,
  spawnAcpAgent,
  startAcpSession,
  waitForReplayContains,
  waitForStructuredView,
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

test("DELETE /acp refuses terminal view and durably stops a structured worker", async ({ spawnServe }) => {
  const { serve, sessionId } = await seedAcpSession(spawnServe, { title: "acp-shutdown" });
  const shutdown = () => fetch(`${serve.baseUrl}/api/sessions/${sessionId}/acp`, { method: "DELETE" });
  expect((await shutdown()).status).toBe(409);
  expect(await isStructured(serve.baseUrl, sessionId)).toBeFalsy();

  expect((await postAcp(serve.baseUrl, sessionId, "/enable")).ok).toBeTruthy();
  await waitForAcpReady(serve.baseUrl, sessionId, 30_000, serve.home);
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

  // #5909: the picker saves to the Session's profile; what the profile inherits stays inherited.
  test("the model picker's checkbox saves the picked model as the Agent's default", async ({ spawnServe, page }) => {
    const { serve, sessionId } = await start(spawnServe, "config-pickers-default");
    const profile = (await listSessions(serve.baseUrl)).find((s) => s.id === sessionId)!.profile as string;
    expect2xx(
      await fetch(`${serve.baseUrl}/api/settings?layer=machine`, {
        method: "PATCH",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ acp: { acp_defaults: { claude: { effort: "high" } } } }),
      }),
    );
    await openSession(page, serve, sessionId);
    await waitForStructuredView(page);
    const picker = page.getByTestId("config-option-model");
    await expect(picker).toContainText("Claude Opus 4.7");
    await picker.click();
    const box = page.getByRole("menuitemcheckbox", { name: "Default for new Claude Code Sessions in Structured view" });
    await expect(box).toHaveAttribute("aria-checked", "false");
    await expect(box).not.toHaveAttribute("aria-disabled", "true");
    await box.click();
    await expect(page.getByText(/is now the default for new Claude Code Sessions in Structured view/)).toBeVisible();

    const read = async (path: string) => (await (await fetch(`${serve.baseUrl}${path}`)).json()).acp.acp_defaults;
    expect(await read(`/api/profiles/${encodeURIComponent(profile)}/settings`)).toEqual({
      claude: { model: "claude-opus-4-7" },
    });
    expect((await read(`/api/settings?profile=${encodeURIComponent(profile)}`)).claude).toMatchObject({
      model: "claude-opus-4-7",
      effort: "high",
    });
    await picker.click();
    await expect(box).toHaveAttribute("aria-checked", "true");
  });

  test("a pin saved after the picker opened refuses the checkbox's save", async ({ spawnServe, page }) => {
    const { serve, sessionId } = await start(spawnServe, "config-pickers-pinned");
    const profile = (await listSessions(serve.baseUrl)).find((s) => s.id === sessionId)!.profile as string;
    const own = `/api/profiles/${encodeURIComponent(profile)}/settings`;
    await openSession(page, serve, sessionId);
    await waitForStructuredView(page);
    const picker = page.getByTestId("config-option-model");
    await expect(picker).toContainText("Claude Opus 4.7");
    await picker.click();
    const box = page.getByRole("menuitemcheckbox", { name: "Default for new Claude Code Sessions in Structured view" });
    await expect(box).toHaveAttribute("aria-checked", "false");
    // The default's read has answered, so the pin below lands after it.
    await expect(box).not.toHaveAttribute("aria-disabled", "true");
    // Settings, in another tab, pins the Agent while this menu is open.
    const pin = { claude: { model: "claude-sonnet-4-6", pin_model: true } };
    expect2xx(
      await fetch(`${serve.baseUrl}/api/settings?profile=${encodeURIComponent(profile)}`, {
        method: "PATCH",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ acp: { acp_defaults: pin } }),
      }),
    );
    await box.click();
    await expect(page.getByText("Claude Code's model is pinned in Agent defaults. Nothing was changed.")).toBeVisible();
    expect((await (await fetch(`${serve.baseUrl}${own}`)).json()).acp.acp_defaults).toEqual(pin);
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
