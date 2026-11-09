import { expect, type Locator, type Page, type TestInfo } from "@playwright/test";
import { readFileSync, existsSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { fakeAcpScriptPath, listSessions, seedSessionViaAoeAdd, type ServeHandle } from "./aoeServe";
import type { ServeOptions } from "./liveTest";

function debugLogPath(home: string): string | undefined {
  // Linux resolves the app dir under XDG_CONFIG_HOME; macOS/Windows under the legacy dot dir.
  return [
    join(home, "config", "agent-of-empires-dev", "debug.log"),
    join(home, ".agent-of-empires-dev", "debug.log"),
  ].find((p) => existsSync(p));
}

function tail(content: string, bytes: number): string {
  return content.length > bytes ? `... (${content.length - bytes} bytes elided)\n` + content.slice(-bytes) : content;
}

function debugLogTail(home: string | undefined): string {
  const path = home && debugLogPath(home);
  try {
    return path ? tail(readFileSync(path, "utf8"), 8_000) : "(debug.log unavailable)";
  } catch {
    return "(debug.log unavailable)";
  }
}

export async function replayFrames(baseUrl: string, sessionId: string): Promise<unknown[]> {
  const replay = await fetch(`${baseUrl}/api/sessions/${sessionId}/acp/replay?since=0`).then((r) => r.json());
  return Array.isArray(replay) ? replay : ((replay?.frames as unknown[]) ?? []);
}

/** The `AgentStartupError` message published when the background spawn fails after enable returned 200. */
function findStartupError(frames: unknown[]): string | null {
  for (const f of frames) {
    const event = (f as { event?: { AgentStartupError?: { message?: string } } } | null)?.event;
    if (event && typeof event === "object" && "AgentStartupError" in event) {
      return event.AgentStartupError?.message ?? "AgentStartupError (no message)";
    }
  }
  return null;
}

async function throwOnStartupError(baseUrl: string, sessionId: string): Promise<number> {
  const frames = await replayFrames(baseUrl, sessionId);
  const startupErr = findStartupError(frames);
  if (startupErr) throw new Error(`acp_enable spawn failed: ${startupErr} (frames=${frames.length})`);
  return frames.length;
}

/**
 * Enable is fire-and-forget, so wait until a replay frame exists (worker up) and the session
 * reports `acp_worker_state === "running"` (handshake done); a prompt sent earlier can be parked.
 */
export async function waitForAcpReady(
  baseUrl: string,
  sessionId: string,
  timeoutMs = 30_000,
  home?: string,
): Promise<void> {
  try {
    await expect
      .poll(() => throwOnStartupError(baseUrl, sessionId), { timeout: timeoutMs, intervals: [100, 200, 200, 200] })
      .toBeGreaterThan(0);
  } catch (err) {
    throw new Error(
      `waitForAcpReady phase 1 (replay frames) failed after ${timeoutMs}ms; the supervisor spawn task is wedged.\n` +
        `Original: ${err instanceof Error ? err.message : String(err)}\n` +
        `--- debug.log tail ---\n${debugLogTail(home)}\n--- end debug.log ---`,
      { cause: err },
    );
  }
  try {
    await expect
      .poll(
        async () => {
          const res = await fetch(`${baseUrl}/api/sessions`);
          if (!res.ok) return "fetch-failed";
          const body = await res.json();
          const sessions: Array<{ id: string; acp_worker_state?: string }> = Array.isArray(body)
            ? body
            : (body.sessions ?? []);
          const state = sessions.find((s) => s.id === sessionId)?.acp_worker_state ?? "absent";
          if (state === "absent") await throwOnStartupError(baseUrl, sessionId);
          return state;
        },
        { timeout: timeoutMs, intervals: [100, 200, 200, 500, 1000] },
      )
      .toBe("running");
  } catch (err) {
    const frames = await replayFrames(baseUrl, sessionId).catch(() => []);
    const sessionsRes = await fetch(`${baseUrl}/api/sessions`).catch(() => null);
    const sessionsBody = sessionsRes ? await sessionsRes.json().catch(() => null) : null;
    const summary = JSON.stringify(
      { sessionId, sessions: sessionsBody, framesCount: frames.length, firstFrames: frames.slice(0, 5) },
      null,
      2,
    );
    throw new Error(
      `waitForAcpReady phase 2 failed: ${err instanceof Error ? err.message : String(err)}\n` +
        `Diagnostic snapshot:\n${summary}\n` +
        `--- debug.log tail ---\n${debugLogTail(home)}\n--- end debug.log ---`,
      { cause: err },
    );
  }
}

/** Poll the replay log until it contains any (or, with `mode: "all"`, every) needle. */
export async function waitForReplayContains(
  baseUrl: string,
  sessionId: string,
  needles: string | string[],
  options: { timeoutMs?: number; mode?: "any" | "all" } = {},
): Promise<void> {
  const list = Array.isArray(needles) ? needles : [needles];
  await expect
    .poll(
      async () => {
        const json = JSON.stringify(await replayFrames(baseUrl, sessionId));
        return options.mode === "all" ? list.every((n) => json.includes(n)) : list.some((n) => json.includes(n));
      },
      { timeout: options.timeoutMs ?? 15_000, intervals: [100, 200, 500, 1000] },
    )
    .toBe(true);
}

export async function enableStructuredViewAndWait(
  baseUrl: string,
  sessionId: string,
  timeoutMs = 30_000,
  home?: string,
): Promise<void> {
  const res = await fetch(`${baseUrl}/api/sessions/${sessionId}/acp/enable`, { method: "POST" });
  if (!res.ok) {
    throw new Error(`structured view enable failed: status=${res.status} body=${await res.text()}`);
  }
  await waitForAcpReady(baseUrl, sessionId, timeoutMs, home);
}

/** Explicitly spawn the agent so its ACP session is attached before a test drives it. */
export async function spawnAcpAgent(baseUrl: string, sessionId: string, agent = "claude"): Promise<void> {
  const res = await fetch(`${baseUrl}/api/sessions/${sessionId}/acp/spawn`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ agent }),
  });
  if (![200, 202, 409].includes(res.status)) throw new Error(`structured view spawn failed: ${res.status}`);
}

export async function postPrompt(baseUrl: string, sessionId: string, text: string): Promise<Response> {
  return postAcp(baseUrl, sessionId, "/prompt", { text });
}

export async function sessionIdByTitle(baseUrl: string, title: string): Promise<string> {
  const session = (await listSessions(baseUrl)).find((s) => s.title === title);
  if (!session) throw new Error(`seeded session '${title}' missing`);
  return session.id;
}

type SpawnServe = (opts?: ServeOptions) => Promise<ServeHandle>;
type AcpSessionOptions = ServeOptions & { title: string; tool?: string };

/** Spawn a structured view server with one seeded session, without enabling structured view on it. */
export async function seedAcpSession(
  spawnServe: SpawnServe,
  { title, tool, ...opts }: AcpSessionOptions,
): Promise<{ serve: ServeHandle; sessionId: string }> {
  const serve = await spawnServe({ acp: true, seedFn: seedSessionViaAoeAdd({ title, tool }), ...opts });
  return { serve, sessionId: await sessionIdByTitle(serve.baseUrl, title) };
}

/** `seedAcpSession`, then enable structured view and wait until the agent is ready. */
export async function startAcpSession(spawnServe: SpawnServe, opts: AcpSessionOptions) {
  const started = await seedAcpSession(spawnServe, opts);
  await enableStructuredViewAndWait(started.serve.baseUrl, started.sessionId, 30_000, started.serve.home);
  return started;
}

export async function replayJson(baseUrl: string, sessionId: string): Promise<string> {
  return JSON.stringify(await replayFrames(baseUrl, sessionId));
}

export async function postAcp(baseUrl: string, sessionId: string, path: string, body?: unknown): Promise<Response> {
  return fetch(`${baseUrl}/api/sessions/${sessionId}/acp${path}`, {
    method: "POST",
    ...(body === undefined ? {} : { headers: { "Content-Type": "application/json" }, body: JSON.stringify(body) }),
  });
}

export const chunk = (text: string) => ({ sessionUpdate: "agent_message_chunk", content: { type: "text", text } });
/** Parks the fake agent's turn until `releaseTurn` or a cancel. */
export const HOLD = { sessionUpdate: "wait_for_release" };
export const endTurn = (...updates: object[]) => ({ updates, stopReason: "end_turn" });
export const script = (...turns: object[]) => ({ turns });

/** Release the fake agent's `wait_for_release` gate for a server spawned with a script object. */
export function releaseTurn(serve: ServeHandle): void {
  writeFileSync(`${fakeAcpScriptPath(serve.home)}.release`, "release");
}

/** The composer textbox in either its idle or mid-turn state. */
export function composer(page: Page): Locator {
  return page.getByRole("textbox", { name: /Send a message|Queue a follow-up/i });
}

export function idleComposer(page: Page): Locator {
  return page.getByRole("textbox", { name: /Send a message/i });
}

export function stopButton(page: Page): Locator {
  return page.getByTestId("composer-actions").getByRole("button", { name: "Stop" });
}

export async function waitForStructuredView(page: Page, timeoutMs = 15_000): Promise<void> {
  await expect(composer(page)).toBeVisible({ timeout: timeoutMs });
}

export async function openSession(page: Page, serve: ServeHandle, sessionId: string): Promise<void> {
  await page.goto(`${serve.baseUrl}/session/${encodeURIComponent(sessionId)}`);
}

/** Navigate to a structured view session and optionally submit a prompt from its composer. */
export async function openStructuredView(
  page: Page,
  serve: ServeHandle,
  sessionId: string,
  prompt?: string,
): Promise<Locator> {
  await openSession(page, serve, sessionId);
  await waitForStructuredView(page);
  const box = composer(page);
  if (prompt !== undefined) {
    await box.fill(prompt);
    await box.press("Enter");
  }
  return box;
}

/** The `<select>` sibling of a FormFields label. */
export function settingsSelectByLabel(page: Page, labelText: string): Locator {
  return page.locator("label").filter({ hasText: labelText }).locator("xpath=..").locator("select").first();
}

export async function openSettingsTab(page: Page, label: string): Promise<void> {
  await page.getByRole("button", { name: label, exact: true }).click();
}

/** Wait until the Profile select resolves, so the profile-driven settings refetch cannot clobber an edit. */
export async function waitForSettingsLoaded(page: Page): Promise<void> {
  const profileSelect = settingsSelectByLabel(page, "Profile");
  await expect(profileSelect).toBeVisible({ timeout: 10_000 });
  await expect.poll(async () => (await profileSelect.inputValue()).length, { timeout: 10_000 }).toBeGreaterThan(0);
}

/** Attach the daemon debug.log tail and fake-acp.log; call before the server's HOME is deleted. */
export async function attachServeDiagnostics(testInfo: TestInfo, serve: { home: string }): Promise<void> {
  const attach = (name: string, read: () => string) => {
    let body: string;
    try {
      body = read();
    } catch (e) {
      return testInfo.attach(`${name}.read-error`, { body: String(e), contentType: "text/plain" });
    }
    return testInfo.attach(name, { body, contentType: "text/plain" });
  };
  const debugLog = debugLogPath(serve.home);
  if (debugLog) await attach("debug.log", () => tail(readFileSync(debugLog, "utf8"), 64_000));
  else
    await testInfo.attach("debug.log.missing", { body: `no debug.log under ${serve.home}`, contentType: "text/plain" });
  const fakeLog = join(serve.home, "fake-acp.log");
  if (existsSync(fakeLog)) await attach("fake-acp.log", () => readFileSync(fakeLog, "utf8"));
  else await testInfo.attach("fake-acp.log.missing", { body: `expected at ${fakeLog}`, contentType: "text/plain" });
}
